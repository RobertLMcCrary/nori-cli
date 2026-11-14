//! Stream a file as `agent_message_chunk`s so performance probes can drive
//! token-sized partial lines and large bursts.
//!
//! * `MOCK_AGENT_STREAM_TEXT_FILE=<path>`: the file to stream.
//! * `MOCK_AGENT_STREAM_CHUNK_BYTES=<n>`: bytes per chunk (default 8, rounded
//!   up to a char boundary).
//! * `MOCK_AGENT_STREAM_DELAY_MS=<n>`: sleep between chunks (default 0).
//!
//! With `MOCK_AGENT_TIMELINE_FILE` set (see [`crate::timeline`]) a `chunk<i>`
//! mark is written right after each chunk was sent, so a probe can compare the
//! send time with the time the glyphs were painted.

use crate::MockAgent;
use crate::acp;
use std::time::Duration;

pub(crate) async fn emit(agent: &MockAgent, session_id: &acp::SessionId) -> Result<(), acp::Error> {
    let Ok(path) = std::env::var("MOCK_AGENT_STREAM_TEXT_FILE") else {
        return Ok(());
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|error| acp::Error::new(-32602, format!("cannot read {path}: {error}")))?;
    let chunk_bytes = std::env::var("MOCK_AGENT_STREAM_CHUNK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or(8);
    let delay = std::env::var("MOCK_AGENT_STREAM_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis);
    let mut start = 0;
    let mut index = 0;
    while start < text.len() {
        let mut end = (start + chunk_bytes).min(text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        agent
            .send_text_chunk(session_id.clone(), &text[start..end])
            .await?;
        crate::timeline::mark(&format!("chunk{index}"));
        index += 1;
        start = end;
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
    }
    Ok(())
}
