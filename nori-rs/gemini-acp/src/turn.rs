use crate::agent::Session;
use acp::Client;
use acp::ConnectionTo;
use acp::UntypedMessage;
use acp::schema::ContentBlock;
use acp::schema::PermissionOption;
use acp::schema::PermissionOptionKind;
use acp::schema::PromptRequest;
use acp::schema::PromptResponse;
use acp::schema::RequestPermissionOutcome;
use acp::schema::RequestPermissionRequest;
use acp::schema::StopReason;
use acp::schema::ToolCallUpdate;
use acp::schema::ToolCallUpdateFields;
use agent_client_protocol as acp;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;

pub(crate) async fn run(
    session: Arc<Session>,
    request: PromptRequest,
    client: ConnectionTo<Client>,
    _turn: tokio::sync::OwnedMutexGuard<()>,
) -> acp::Result<PromptResponse> {
    let id = request.session_id.to_string();
    let mut parts = Vec::new();
    for part in request.prompt {
        match part {
            ContentBlock::Text(text) => {
                if text
                    .annotations
                    .as_ref()
                    .and_then(|annotations| annotations.audience.as_ref())
                    .is_some_and(|audience| !audience.contains(&acp::schema::Role::Assistant))
                {
                    continue;
                }
                parts.push(json!({"text":text.text}));
            }
            ContentBlock::Image(image) if !image.data.is_empty() => {
                parts.push(json!({"inlineData":{"mimeType":image.mime_type, "data":image.data}}))
            }
            ContentBlock::Resource(resource) => match resource.resource {
                acp::schema::EmbeddedResourceResource::TextResourceContents(text) => {
                    parts.push(json!({"text":format!("[{}]\n{}", text.uri, text.text)}))
                }
                acp::schema::EmbeddedResourceResource::BlobResourceContents(blob)
                    if blob.mime_type.is_some() =>
                {
                    parts.push(json!({"inlineData":{"mimeType":blob.mime_type, "data":blob.blob}}))
                }
                _ => return Err(acp::Error::invalid_params().data("Unsupported embedded resource")),
            },
            _ => {
                return Err(acp::Error::invalid_params()
                    .data("Gemini adapter cannot use this prompt content"));
            }
        }
    }
    let start = session.prompt_start.lock().await;
    if session.prompt_state.lock().expect("prompt state").cancelled {
        return Ok(PromptResponse::new(StopReason::Cancelled));
    }
    session
        .engine
        .send(&json!({"type":"prompt", "parts":parts}))
        .await
        .map_err(acp::Error::into_internal_error)?;
    session.prompt_state.lock().expect("prompt state").sent = true;
    drop(start);
    loop {
        let event = session
            .engine
            .receive()
            .await
            .map_err(acp::Error::into_internal_error)?;
        match event["type"].as_str() {
            Some("content") => notify(
                &client,
                &id,
                json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":event["value"]}}),
            )?,
            Some("thought") => {
                if let Some(text) = event["value"]["description"].as_str() {
                    notify(
                        &client,
                        &id,
                        json!({"sessionUpdate":"agent_thought_chunk", "content":{"type":"text", "text":text}}),
                    )?;
                }
            }
            Some("tool") => {
                let tool_id = &event["callId"];
                let title = event["display"]["name"]
                    .as_str()
                    .or_else(|| event["name"].as_str())
                    .unwrap_or("Tool");
                let status = event["status"].as_str().unwrap_or("pending");
                let update = if status == "pending" {
                    json!({"sessionUpdate":"tool_call", "toolCallId":tool_id, "title":title, "status":"pending", "rawInput":event["args"]})
                } else {
                    let mapped = match status {
                        "executing" => "in_progress",
                        "success" => "completed",
                        _ => "failed",
                    };
                    let mut update = json!({"sessionUpdate":"tool_call_update", "toolCallId":tool_id, "status":mapped, "rawOutput":event["result"]});
                    let content = crate::tool_output::content(&event["result"]);
                    if !content.is_empty() {
                        update["content"] = content.into();
                    }
                    update
                };
                notify(&client, &id, update)?;
            }
            Some("permission") => {
                let session = session.clone();
                let client = client.clone();
                let session_id = request.session_id.clone();
                tokio::spawn(async move {
                    let title = event["details"]["title"]
                        .as_str()
                        .or_else(|| event["name"].as_str())
                        .unwrap_or("Gemini tool");
                    let tool = ToolCallUpdate::new(
                        event["callId"].as_str().unwrap_or_default().to_owned(),
                        ToolCallUpdateFields::new().title(title.to_owned()),
                    );
                    let options = vec![
                        PermissionOption::new(
                            "allow",
                            "Allow once",
                            PermissionOptionKind::AllowOnce,
                        ),
                        PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
                    ];
                    let response = client
                        .send_request(RequestPermissionRequest::new(session_id, tool, options))
                        .block_task()
                        .await;
                    let allow = matches!(response, Ok(response) if matches!(response.outcome, RequestPermissionOutcome::Selected(ref selected) if selected.option_id.to_string() == "allow"));
                    let _ = session
                        .engine
                        .send(&json!({"type":"permission", "id":event["id"], "allow":allow}))
                        .await;
                });
            }
            Some("done") => {
                return Ok(PromptResponse::new(match event["reason"].as_str() {
                    Some("cancelled") => StopReason::Cancelled,
                    Some("max_tokens") => StopReason::MaxTokens,
                    Some("max_turn_requests") => StopReason::MaxTurnRequests,
                    Some("refusal") => StopReason::Refusal,
                    _ => StopReason::EndTurn,
                }));
            }
            Some("error") => {
                return Err(acp::Error::internal_error().data(event["message"].clone()));
            }
            _ => return Err(acp::Error::internal_error().data("Unexpected Gemini engine event")),
        }
    }
}

fn notify(client: &ConnectionTo<Client>, id: &str, update: Value) -> acp::Result<()> {
    client.send_notification(UntypedMessage::new(
        "session/update",
        json!({"sessionId":id, "update":update}),
    )?)
}
