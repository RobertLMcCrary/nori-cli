use std::time::Duration;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TuiSession;

fn without_csi(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"\x1b[") {
            index += 2;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    output
}

#[test]
fn native_history_uses_soft_wrap_without_replaying_on_resize() {
    let paragraph = "REFLOWSTART The **bold café 界** text continues through a long paragraph with enough words to wrap naturally in the terminal while preserving each character and its formatting. More Unicode λ and 日本語 continue the same logical paragraph without a forced line break. REFLOWEND";
    let mut session =
        TuiSession::spawn_with_config(30, 80, SessionConfig::new().with_mock_response(paragraph))
            .expect("start TUI");
    session
        .wait_for_text("›", Duration::from_secs(30))
        .expect("prompt");
    session.start_output_capture();
    session
        .submit_input("test native paragraph")
        .expect("prompt");
    session
        .wait_for_text("REFLOWEND", Duration::from_secs(30))
        .expect("response completes");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let screen = session.screen_contents();
        if let Some(row) = screen.lines().position(|line| line.contains("REFLOWSTART"))
            && session.screen_row_soft_wrapped(row as u16)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "committed paragraph must use terminal soft wrapping: {screen}"
        );
        session.poll().expect("read terminal");
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = session.output_bytes().expect("captured output");
    assert!(
        output
            .windows("café 界".len())
            .any(|part| part == "café 界".as_bytes()),
        "Unicode survives output"
    );
    assert!(
        output
            .windows(b"\x1b[1m".len())
            .any(|part| part == b"\x1b[1m"),
        "bold text keeps its terminal style"
    );
    let plain = String::from_utf8_lossy(&without_csi(output)).into_owned();
    let expected = paragraph.replace("**", "");
    assert!(
        plain.contains(&expected),
        "the entire logical paragraph is emitted without inserted breaks"
    );
    session.start_output_capture();
    for width in [35, 100] {
        session.resize(30, width).expect("resize");
        session
            .wait_for_text("›", Duration::from_secs(10))
            .expect("prompt remains visible");
        session.poll().expect("collect output");
    }
    let output = session.output_bytes().expect("captured output");
    let occurrences = output
        .windows(b"REFLOWSTART".len())
        .filter(|part| *part == b"REFLOWSTART")
        .count();
    assert_eq!(occurrences, 0, "history must not be replayed on resize");
}
