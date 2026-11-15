//! A burst of code is rendered over frames, also when the turn ends with it.

use super::*;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;

fn answer(text: &str) -> nori_protocol::SessionEvent {
    nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
        acp::AgentNotification::SessionNotification(
            serde_json::from_value(serde_json::json!({
                "sessionId": "session",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": text },
                },
            }))
            .expect("notification"),
        ),
    ))
}

fn code_lines(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> Vec<String> {
    drain_insert_history(rx)
        .iter()
        .flat_map(|lines| lines.iter().map(std::string::ToString::to_string))
        .filter(|line| line.contains("value_"))
        .collect()
}

#[test]
fn a_burst_that_ends_its_turn_is_finished_by_the_frames_that_follow() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    let generation = chat.session_generation;
    let request_id = acp::RequestId::Str("prompt".to_string());
    chat.handle_session_event(
        generation,
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionPhaseChanged(
            nori_protocol::SessionPhase::Prompting {
                request_id: request_id.clone(),
            },
        )),
    );
    let body: String = (0..2_000)
        .map(|index| format!("let value_{index:04} = {index};\n"))
        .collect();
    chat.handle_session_event(generation, answer(&format!("```rust\n{body}```\n")));
    chat.handle_session_event(
        generation,
        nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response {
            request_id,
            response: Ok(acp::AgentResponse::PromptResponse(
                acp::PromptResponse::new(acp::StopReason::EndTurn),
            )),
        }),
    );

    // The turn's end did not render the whole burst at once.
    let mut lines = code_lines(&mut rx);
    assert!(
        lines.len() < 2_000,
        "{} lines at the end of the turn",
        lines.len()
    );
    let mut frames = 0;
    while chat.has_active_agent_stream() {
        chat.commit_stream_for_frame();
        lines.extend(code_lines(&mut rx));
        frames += 1;
        assert!(frames < 100, "the burst never finished");
    }

    assert!(frames > 2, "{frames} frames");
    let expected: Vec<String> = (0..2_000)
        .map(|index| format!("let value_{index:04} = {index};"))
        .collect();
    let lines: Vec<String> = lines
        .iter()
        .map(|line| line.trim_start_matches(['•', ' ']).trim_end().to_string())
        .collect();
    assert_eq!(lines, expected);
}
