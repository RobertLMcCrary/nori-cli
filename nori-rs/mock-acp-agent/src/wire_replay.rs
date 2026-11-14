//! Replay recorded or hand-written agent wire traffic as a prompt's response.
//!
//! `MOCK_AGENT_WIRE_REPLAY=<path>` names a JSONL file. Each line is either a recorded wire
//! message, `{"t_ms": 12.5, "dir": "in", "msg": {...}}`, of which the `session/update`
//! notifications are sent, or a hand-written step:
//!
//! * `{"t_ms": 40, "update": {...}, "session": "child-1"}` sends a `session/update` for that
//!   session (`"root"` or omitted is the prompt's own session);
//! * `{"t_ms": 40, "permission": {"session": "child-1", "tool_call_id": "p1", "title": "..."}}`
//!   asks the client for permission on that session and waits for the answer.
//!
//! The root session id the recording used is replaced with the real one wherever it appears.
//! `MOCK_AGENT_REPLAY_SCALE` scales the recorded delays (`0` plays everything back at once).
//! The prompt ends when the file does, or is cancelled when a cancel arrives first.

use std::time::Duration;

use agent_client_protocol::UntypedMessage;
use serde_json::Value;
use serde_json::json;

use crate::MockAgent;
use crate::acp;

/// Play the file back; `None` when no replay is configured.
pub(crate) async fn run(
    agent: &MockAgent,
    session_id: &acp::SessionId,
) -> Option<Result<acp::PromptResponse, acp::Error>> {
    let path = std::env::var("MOCK_AGENT_WIRE_REPLAY").ok()?;
    Some(play(agent, session_id, &path).await)
}

async fn play(
    agent: &MockAgent,
    session_id: &acp::SessionId,
    path: &str,
) -> Result<acp::PromptResponse, acp::Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| acp::Error::new(-32602, format!("cannot read {path}: {error}")))?;
    let steps = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .map_err(|error| acp::Error::new(-32602, format!("bad replay line: {error}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut replay = Replay {
        real_root: session_id.to_string(),
        recorded_root: None,
        previous_t: 0.0,
        scale: std::env::var("MOCK_AGENT_REPLAY_SCALE")
            .ok()
            .and_then(|scale| scale.parse().ok())
            .unwrap_or(1.0),
    };
    // Steps marked `"after_response": true` and everything after them play once the prompt has
    // ended: children that outlive the turn.
    let split = steps
        .iter()
        .position(|step| step.get("after_response").and_then(Value::as_bool) == Some(true))
        .unwrap_or(steps.len());
    let (during, after) = steps.split_at(split);
    if let Some(cancelled) = replay.play_steps(agent, during).await? {
        return Ok(cancelled);
    }
    if !after.is_empty() {
        let background = MockAgent {
            cx: agent.cx.clone(),
            state: agent.state.clone(),
        };
        let after = after.to_vec();
        tokio::spawn(async move {
            let _ = replay.play_steps(&background, &after).await;
        });
    }
    timeline_end();
    Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
}

struct Replay {
    real_root: String,
    recorded_root: Option<String>,
    previous_t: f64,
    scale: f64,
}

impl Replay {
    /// Play `steps`; `Some` is the response when a cancel ended the prompt early.
    async fn play_steps(
        &mut self,
        agent: &MockAgent,
        steps: &[Value],
    ) -> Result<Option<acp::PromptResponse>, acp::Error> {
        for step in steps {
            let t_ms = step
                .get("t_ms")
                .and_then(Value::as_f64)
                .unwrap_or(self.previous_t);
            let wait = (t_ms - self.previous_t).max(0.0) * self.scale;
            self.previous_t = t_ms;
            if wait >= 1.0 {
                tokio::time::sleep(Duration::from_micros((wait * 1000.0) as u64)).await;
            }
            if agent.cancel_requested() {
                return Ok(Some(acp::PromptResponse::new(acp::StopReason::Cancelled)));
            }
            if let Some(permission) = step.get("permission") {
                self.ask_permission(agent, permission).await;
                continue;
            }
            let Some((params, is_recorded)) = Self::params_of(step) else {
                continue;
            };
            let mut serialized = params.to_string();
            if is_recorded {
                if self.recorded_root.is_none() {
                    self.recorded_root = params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                if let Some(recorded) = &self.recorded_root {
                    serialized = serialized.replace(recorded.as_str(), &self.real_root);
                }
            } else {
                serialized = serialized.replace("\"root\"", &format!("\"{}\"", self.real_root));
            }
            let params: Value = serde_json::from_str(&serialized)
                .map_err(|error| acp::Error::new(-32603, format!("bad replay params: {error}")))?;
            agent
                .cx
                .send_notification(UntypedMessage::new("session/update", params)?)?;
        }
        Ok(None)
    }

    fn params_of(step: &Value) -> Option<(Value, bool)> {
        match (step.get("msg"), step.get("update")) {
            (Some(msg), _) => {
                if step.get("dir").and_then(Value::as_str) != Some("in")
                    || msg.get("method").and_then(Value::as_str) != Some("session/update")
                {
                    return None;
                }
                Some((msg.get("params").cloned().unwrap_or(Value::Null), true))
            }
            (None, Some(update)) => {
                let session = step
                    .get("session")
                    .and_then(Value::as_str)
                    .unwrap_or("root");
                Some((json!({"sessionId": session, "update": update}), false))
            }
            (None, None) => None,
        }
    }

    async fn ask_permission(&self, agent: &MockAgent, permission: &Value) {
        let session = permission
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or("root");
        let session = if session == "root" {
            self.real_root.as_str()
        } else {
            session
        };
        let tool_call = acp::ToolCallUpdate::new(
            acp::ToolCallId::new(
                permission
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("permission-1")
                    .to_string(),
            ),
            acp::ToolCallUpdateFields::new()
                .title(
                    permission
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("Run a command")
                        .to_string(),
                )
                .kind(acp::ToolKind::Execute),
        );
        let options = vec![
            acp::PermissionOption::new("allow", "Allow", acp::PermissionOptionKind::AllowOnce),
            acp::PermissionOption::new("deny", "Deny", acp::PermissionOptionKind::RejectOnce),
        ];
        let _ = agent
            .request_permission_via_client(
                acp::SessionId::new(session.to_string()),
                tool_call,
                options,
            )
            .await;
    }
}

fn timeline_end() {
    crate::timeline::mark("replay_done");
}
