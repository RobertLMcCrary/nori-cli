#![cfg(unix)]

use std::time::Duration;
use tui_pty_e2e::Key;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TuiSession;

#[test]
fn question_accepts_text_and_escape_cancels_without_stalling_the_agent() {
    let mut session = TuiSession::spawn(25, 80).expect("start Nori");
    session.wait_for_text("›", TIMEOUT).expect("prompt");
    session
        .submit_input("fixture:user-question")
        .expect("request question");
    session
        .wait_for_text("Proceed?", Duration::from_secs(15))
        .expect("question visible");
    session.send_str("approved").expect("type answer");
    session.send_key(Key::Enter).expect("submit answer");
    session
        .wait_for_text("approved", Duration::from_secs(15))
        .expect("answer returned to agent");
    session
        .wait_for_text("question-response", Duration::from_secs(15))
        .expect("agent resumed");
    session
        .submit_input("fixture:user-question")
        .expect("request second question");
    session
        .wait_for_text("Proceed?", Duration::from_secs(15))
        .expect("second question visible");
    session.send_key(Key::Escape).expect("cancel question");
    session
        .wait_for_text("cancelled", Duration::from_secs(15))
        .expect("agent received cancellation");
}
