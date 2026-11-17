//! Translate one OpenCode v2 execution into ordered ACP updates.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use acp::Client;
use acp::ConnectionTo;
use acp::UntypedMessage;
use acp::schema::ContentBlock;
use acp::schema::PromptRequest;
use acp::schema::PromptResponse;
use acp::schema::StopReason;
use agent_client_protocol as acp;
use futures::StreamExt;
use reqwest::Method;
use serde_json::Value;
use serde_json::json;

use crate::agent::Session;

pub(crate) async fn run(
    session: Arc<Session>,
    request: PromptRequest,
    client: ConnectionTo<Client>,
    known_children: Arc<tokio::sync::Mutex<HashMap<String, Arc<Session>>>>,
    turn: tokio::sync::OwnedMutexGuard<()>,
) -> acp::Result<PromptResponse> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut response_tx = Some(response_tx);
        let result = run_with_children(
            session,
            request,
            client,
            &mut response_tx,
            known_children,
            turn,
        )
        .await;
        if let Some(response_tx) = response_tx {
            let _ = response_tx.send(result);
        }
    });
    response_rx.await.map_err(acp::Error::into_internal_error)?
}

async fn run_with_children(
    session: Arc<Session>,
    request: PromptRequest,
    client: ConnectionTo<Client>,
    response_tx: &mut Option<tokio::sync::oneshot::Sender<acp::Result<PromptResponse>>>,
    known_children: Arc<tokio::sync::Mutex<HashMap<String, Arc<Session>>>>,
    turn: tokio::sync::OwnedMutexGuard<()>,
) -> acp::Result<PromptResponse> {
    let mut turn = Some(turn);
    let mut text = Vec::new();
    let mut files = Vec::new();
    let mut synthetic = Vec::new();
    for part in request.prompt {
        match part {
            ContentBlock::Text(part) => {
                let audience = part
                    .annotations
                    .as_ref()
                    .and_then(|annotations| annotations.audience.as_ref());
                if audience
                    .is_some_and(|audience| !audience.contains(&acp::schema::Role::Assistant))
                {
                    continue;
                }
                if audience.is_some_and(|audience| !audience.contains(&acp::schema::Role::User)) {
                    synthetic.push(part.text);
                } else {
                    text.push(part.text);
                }
            }
            ContentBlock::Image(part) => {
                let uri = if part.data.is_empty() {
                    part.uri.ok_or_else(|| {
                        acp::Error::invalid_params().data("Image has no data or URI")
                    })?
                } else {
                    format!("data:{};base64,{}", part.mime_type, part.data)
                };
                if uri.starts_with("data:") || uri.starts_with("file://") {
                    files.push(json!({"uri":uri,"name":"image"}));
                } else {
                    text.push(format!("[image]({uri})"));
                }
            }
            ContentBlock::Resource(part) => match part.resource {
                acp::schema::EmbeddedResourceResource::TextResourceContents(resource) => {
                    text.push(format!("[{}]\n{}", resource.uri, resource.text))
                }
                acp::schema::EmbeddedResourceResource::BlobResourceContents(resource) => {
                    if let Some(mime) = resource.mime_type {
                        let uri = if resource.uri.starts_with("data:") {
                            resource.uri
                        } else {
                            format!("data:{mime};base64,{}", resource.blob)
                        };
                        files.push(json!({"uri":uri,"name":"file"}));
                    }
                }
                _ => return Err(acp::Error::invalid_params().data("Unsupported embedded resource")),
            },
            ContentBlock::ResourceLink(part) if part.uri.starts_with("file://") => {
                files.push(json!({"uri":part.uri,"name":part.name}))
            }
            ContentBlock::ResourceLink(part) => text.push(format!("[{}]({})", part.name, part.uri)),
            ContentBlock::Audio(_) => {}
            _ => return Err(acp::Error::invalid_params().data("Unsupported prompt content")),
        }
    }
    let id = request.session_id.to_string();
    let mut events = session
        .server
        .events()
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    if !synthetic.is_empty() {
        session.server.request(Method::POST, &["session", &id, "synthetic"], Some(&json!({"text":synthetic.join("\n\n"),"description":"ACP embedded context","delivery":"steer","resume":false})))
            .await.map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    }
    let text = text.join("\n");
    let command = text.strip_prefix('/').and_then(|text| {
        let (name, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        if name.is_empty() {
            None
        } else {
            Some((name.to_owned(), args.trim_start().to_owned()))
        }
    });
    let known_command = if let Some((name, _)) = &command {
        session
            .catalog
            .lock()
            .await
            .commands()
            .iter()
            .any(|item| item["name"] == *name)
    } else {
        false
    };
    let compact_id = if command.as_ref().is_some_and(|(name, _)| name == "compact") {
        Some(format!("msg_{}", uuid::Uuid::new_v4().simple()))
    } else {
        None
    };
    let (method, body) = if let Some(compact_id) = &compact_id {
        ("compact", json!({"id":compact_id}))
    } else if known_command {
        let (name, args) = command.expect("known command");
        (
            "command",
            json!({"name":name,"text":args,"files":files,"delivery":"steer"}),
        )
    } else {
        (
            "prompt",
            json!({"text":text,"files":files,"delivery":"steer"}),
        )
    };
    let prompt_path = ["session", &id, method];
    let result = tokio::select! {
        result = session.server.request(Method::POST, &prompt_path, Some(&body)) => result.map_err(|e| acp::Error::internal_error().data(e.to_string()))?,
        _ = session.cancel_notify.notified() => return Ok(PromptResponse::new(StopReason::Cancelled)),
    };
    let message_id = if let Some(id) = compact_id.as_deref() {
        Some(id)
    } else if known_command {
        None
    } else {
        Some(result["data"]["id"].as_str().ok_or_else(|| {
            acp::Error::internal_error().data("OpenCode did not return the submitted message id")
        })?)
    };
    let mut started = false;
    let mut children = HashMap::<String, (String, i64, Option<String>)>::new();
    let mut tools = HashMap::<(String, String), (String, Value)>::new();
    let mut open_children = HashSet::new();
    let mut running_children = HashSet::new();
    let mut terminal = None;
    let mut step_error = None::<Value>;
    let mut finish = None::<String>;
    let mut compactions = HashMap::<String, String>::new();
    let mut usage = [0_i64; 5];
    let mut last_usage = None::<i64>;
    let mut cancel_since = None::<std::time::Instant>;
    loop {
        let event = tokio::select! {
            event = events.next() => match event { Some(event) => event, None => break },
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                if session.closed.load(Ordering::SeqCst) {
                    return Ok(PromptResponse::new(StopReason::Cancelled));
                }
                if response_tx.is_some() && session.cancelled.load(Ordering::SeqCst) {
                    let since = cancel_since.get_or_insert_with(std::time::Instant::now);
                    if !started || since.elapsed() >= std::time::Duration::from_secs(5) {
                        return Ok(PromptResponse::new(StopReason::Cancelled));
                    }
                }
                continue;
            }
        };
        let event = event.map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        let value: Value =
            serde_json::from_str(&event.data).map_err(acp::Error::into_internal_error)?;
        let Some(kind) = value["type"].as_str() else {
            continue;
        };
        let data = &value["data"];
        let event_session = data["sessionID"]
            .as_str()
            .or_else(|| data["form"]["sessionID"].as_str())
            .unwrap_or_default();
        if kind == "session.created" {
            if event_session.is_empty() || children.contains_key(event_session) {
                continue;
            }
            let Some(parent) = data["parentID"].as_str() else {
                continue;
            };
            let depth = if parent == id && response_tx.is_some() {
                1
            } else if let Some((_, depth, _)) = children.get(parent) {
                depth + 1
            } else {
                continue;
            };
            let title = data["title"].as_str().map(ToOwned::to_owned);
            known_children
                .lock()
                .await
                .insert(event_session.to_owned(), session.clone());
            children.insert(
                event_session.to_owned(),
                (parent.to_owned(), depth, title.clone()),
            );
            open_children.insert(event_session.to_owned());
            send_child(&client, parent, event_session, title.as_deref(), "created")?;
            // Publish the child's own controls after its identity is announced.
            // Loading the catalog is independent of the parent event stream.
            let child_id = event_session.to_owned();
            let server = session.server.clone();
            let child_client = client.clone();
            tokio::spawn(async move {
                let result = async {
                    let child = server.request(Method::GET, &["session", &child_id], None).await.map_err(|error| error.to_string())?;
                    let catalog = crate::config::Catalog::load(&server, &child["data"]).await.map_err(|error| error.to_string())?;
                    let options = catalog.options().map_err(|error| error.to_string())?;
                    let event = UntypedMessage::new("session/update", json!({"sessionId":child_id,"update":{"sessionUpdate":"config_option_update","configOptions":options}})).map_err(|error| error.to_string())?;
                    child_client.send_notification(event).map_err(|error| error.to_string())
                }.await;
                if let Err(error) = result {
                    tracing::warn!(%error, "Failed to load native child configuration");
                }
            });
            continue;
        }
        // Once the parent response has been sent, this subscriber only tracks
        // descendants already associated with that turn.
        if response_tx.is_none() && event_session == id {
            continue;
        }
        if event_session == id
            && kind == "session.inbox.delivered"
            && (message_id.is_none() || data["inboxID"].as_str() == message_id)
        {
            started = true;
        }
        if event_session == id
            && kind == "session.compaction.started"
            && data["inputID"].as_str() == message_id
            && compact_id.is_some()
        {
            started = true;
        }
        if !started {
            continue;
        }
        let child = children.get(event_session);
        if event_session != id && child.is_none() {
            continue;
        }
        if event_session == id
            && kind == "session.retry.scheduled"
            && crate::provider_error::is_exhausted(&data["error"])
        {
            let error = crate::provider_error::data(&data["error"]);
            let _ = session
                .server
                .request(
                    Method::POST,
                    &["session", &id, "interrupt"],
                    Some(&json!({})),
                )
                .await;
            if session.cancelled.load(Ordering::SeqCst) {
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
            return Err(acp::Error::internal_error().data(error));
        }
        if event_session == id
            && matches!(
                kind,
                "session.compaction.started"
                    | "session.compaction.ended"
                    | "session.compaction.failed"
            )
        {
            let message_id = if kind == "session.compaction.started" {
                let message_id = data["inputID"]
                    .as_str()
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        value["id"]
                            .as_str()
                            .map(|id| id.replacen("evt_", "msg_", 1))
                    })
                    .unwrap_or_default();
                compactions.insert(id.clone(), message_id.clone());
                message_id
            } else {
                compactions.remove(&id).unwrap_or_default()
            };
            let status = match kind {
                "session.compaction.started" => "started",
                "session.compaction.ended" => "completed",
                _ => "failed",
            };
            client.send_notification(UntypedMessage::new("session/update", json!({"sessionId":id,"update":{"sessionUpdate":"session_info_update","_meta":{"opencode/compaction":{"status":status,"messageId":message_id,"reason":data["reason"]}}}}))?)?;
            continue;
        }
        if kind == "permission.asked" {
            let server = session.clone();
            let client = client.clone();
            let client_session = request.session_id.clone();
            let event = data.clone();
            let child = child.map(|(parent, depth, title)| {
                (
                    event_session.to_owned(),
                    parent.clone(),
                    *depth,
                    title.clone(),
                )
            });
            tokio::spawn(async move {
                crate::permission::ask_permission(server, client, client_session, event, child)
                    .await;
            });
            continue;
        }
        if kind == "form.created" {
            if data["form"]["metadata"]["kind"] == "question" {
                let session = session.clone();
                let client = client.clone();
                let form = data["form"].clone();
                tokio::spawn(async move {
                    crate::question::ask(session, client, form).await;
                });
                continue;
            }
            if let (Some(form_session), Some(form_id)) = (
                data["form"]["sessionID"].as_str(),
                data["form"]["id"].as_str(),
            ) {
                session
                    .server
                    .request(
                        Method::DELETE,
                        &["session", form_session, "form", form_id],
                        None,
                    )
                    .await
                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
            }
            continue;
        }
        if let Some((parent, _depth, title)) = child {
            let status = match kind {
                "session.execution.started" => Some("running"),
                "session.execution.succeeded" => Some("completed"),
                "session.execution.failed" => Some("failed"),
                "session.execution.interrupted" => Some("interrupted"),
                _ => None,
            };
            if let Some(status) = status {
                if status == "running" {
                    open_children.insert(event_session.to_owned());
                    if running_children.insert(event_session.to_owned()) {
                        send_child(&client, parent, event_session, title.as_deref(), status)?;
                    }
                } else if open_children.remove(event_session) {
                    running_children.remove(event_session);
                    send_child(&client, parent, event_session, title.as_deref(), status)?;
                }
            }
        } else {
            if matches!(kind, "session.step.ended" | "session.step.failed")
                && data["tokens"].is_object()
            {
                let tokens = &data["tokens"];
                let step = [
                    tokens["input"].as_i64().unwrap_or_default(),
                    tokens["output"].as_i64().unwrap_or_default(),
                    tokens["reasoning"].as_i64().unwrap_or_default(),
                    tokens["cache"]["read"].as_i64().unwrap_or_default(),
                    tokens["cache"]["write"].as_i64().unwrap_or_default(),
                ];
                for (total, count) in usage.iter_mut().zip(step) {
                    *total = total.saturating_add(count);
                }
                last_usage = Some(step.into_iter().fold(0_i64, i64::saturating_add));
            }
            match kind {
                "session.step.started" => step_error = None,
                "session.step.failed" => step_error = Some(data["error"].clone()),
                "session.step.ended" => finish = data["finish"].as_str().map(ToOwned::to_owned),
                "session.execution.succeeded" => {
                    terminal = Some(if let Some(error) = &step_error {
                        match error["type"].as_str() {
                            Some("provider.auth") => Err(acp::Error::auth_required()),
                            Some("aborted") => Ok(PromptResponse::new(StopReason::Cancelled)),
                            Some("provider.content-filter") => {
                                Ok(PromptResponse::new(StopReason::Refusal))
                            }
                            _ => Err(acp::Error::internal_error()
                                .data(crate::provider_error::data(error))),
                        }
                    } else {
                        let reason = match finish.as_deref() {
                            Some("length") => StopReason::MaxTokens,
                            Some("content-filter") => StopReason::Refusal,
                            _ => StopReason::EndTurn,
                        };
                        Ok(PromptResponse::new(reason))
                    });
                }
                "session.execution.interrupted" => {
                    terminal = Some(Ok(PromptResponse::new(StopReason::Cancelled)))
                }
                "session.execution.failed" => {
                    terminal = Some(match data["error"]["type"].as_str() {
                        Some("provider.auth") => Err(acp::Error::auth_required()),
                        Some("aborted") => Ok(PromptResponse::new(StopReason::Cancelled)),
                        Some("provider.content-filter") => {
                            Ok(PromptResponse::new(StopReason::Refusal))
                        }
                        _ => Err(acp::Error::internal_error()
                            .data(crate::provider_error::data(&data["error"]))),
                    })
                }
                _ => {}
            }
        }
        let mut enriched = data.clone();
        if let Some(tool_id) = data["id"].as_str() {
            let key = (event_session.to_owned(), tool_id.to_owned());
            if kind == "session.tool.input.started" {
                tools.insert(
                    key.clone(),
                    (
                        data["name"].as_str().unwrap_or("Tool").to_owned(),
                        json!({}),
                    ),
                );
            }
            if let Some((name, input)) = tools.get_mut(&key) {
                if kind == "session.tool.called" {
                    *input = data["input"].clone();
                }
                enriched["name"] = name.clone().into();
                if kind == "session.tool.progress" {
                    enriched["input"] = input.clone();
                }
            }
            if matches!(kind, "session.tool.success" | "session.tool.failed") {
                tools.remove(&key);
            }
        }
        if let Some(update) = crate::event::update_for_event(kind, &enriched) {
            // A child's updates use its own session ID, allowing the host to route them
            // without guessing from titles, tool status, or private metadata.
            client.send_notification(UntypedMessage::new(
                "session/update",
                json!({"sessionId":event_session,"update":update}),
            )?)?;
        }
        if let Some(result) = terminal.take() {
            let response = if session.cancelled.load(Ordering::SeqCst) {
                PromptResponse::new(StopReason::Cancelled)
            } else {
                result?
            };
            if let Some(used) = last_usage {
                if let Some(size) = session.catalog.lock().await.context_limit() {
                    client.send_notification(UntypedMessage::new("session/update", json!({"sessionId":id,"update":{"sessionUpdate":"usage_update","used":used,"size":size}}))?)?;
                }
                let mut response =
                    serde_json::to_value(response).map_err(acp::Error::into_internal_error)?;
                response["usage"] = json!({"inputTokens":usage[0],"outputTokens":usage[1],"thoughtTokens":usage[2],"cachedReadTokens":usage[3],"cachedWriteTokens":usage[4],"totalTokens":usage.into_iter().fold(0_i64, i64::saturating_add)});
                let response =
                    serde_json::from_value(response).map_err(acp::Error::into_internal_error)?;
                if open_children.is_empty() {
                    return Ok(response);
                }
                drop(turn.take());
                if let Some(response_tx) = response_tx.take() {
                    let _ = response_tx.send(Ok(response));
                }
                continue;
            }
            if open_children.is_empty() {
                return Ok(response);
            }
            drop(turn.take());
            if let Some(response_tx) = response_tx.take() {
                let _ = response_tx.send(Ok(response));
            }
        }
        if response_tx.is_none() && open_children.is_empty() {
            return Ok(PromptResponse::new(StopReason::EndTurn));
        }
    }
    Err(acp::Error::internal_error().data("OpenCode event stream ended during a prompt"))
}

fn send_child(
    client: &ConnectionTo<Client>,
    parent: &str,
    child: &str,
    title: Option<&str>,
    status: &str,
) -> acp::Result<()> {
    let state = match status {
        "created" => None,
        "running" => Some(json!({"state":"running"})),
        "completed" => Some(json!({"state":"idle", "stopReason":"end_turn"})),
        "failed" => Some(json!({"state":"failed"})),
        "interrupted" => Some(json!({"state":"cancelled"})),
        _ => return Err(acp::Error::internal_error().data("Unknown OpenCode child state")),
    };
    let mut update = json!({"sessionUpdate":"subagent_update", "sessionId":child});
    if let Some(state) = state {
        update["state"] = state;
    }
    if let Some(title) = title {
        update["title"] = title.into();
    }
    client.send_notification(UntypedMessage::new(
        "session/update",
        json!({"sessionId":parent, "update":update}),
    )?)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use crate::event::update_for_event;

    #[test]
    fn maps_text_reasoning_and_tool_results() {
        let text = json!({"assistantMessageID":"msg_1", "delta":"hello"});
        assert_eq!(
            update_for_event("session.text.delta", &text),
            Some(
                json!({"sessionUpdate":"agent_message_chunk", "messageId":"msg_1", "content":{"type":"text","text":"hello"}})
            )
        );
        let thought = json!({"assistantMessageID":"msg_1", "ordinal":2, "delta":"thinking"});
        assert_eq!(
            update_for_event("session.reasoning.delta", &thought),
            Some(
                json!({"sessionUpdate":"agent_thought_chunk", "messageId":"msg_1:reasoning:2", "content":{"type":"text","text":"thinking"}})
            )
        );
        let result = json!({"id":"tool_1", "content":[{"type":"text","text":"done"}], "metadata":{"ok":true}});
        assert_eq!(
            update_for_event("session.tool.success", &result),
            Some(
                json!({"sessionUpdate":"tool_call_update", "toolCallId":"tool_1", "status":"completed", "content":[{"type":"content","content":{"type":"text","text":"done"}}], "rawOutput":{"id":"tool_1", "content":[{"type":"text","text":"done"}], "metadata":{"ok":true}}})
            )
        );
    }
}
