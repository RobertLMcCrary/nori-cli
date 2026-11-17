//! Pkl conversation-program dispatch.
//!
//! This module is deliberately a thin adapter over the existing ACP backend
//! and orchestrator. The Pkl evaluator only produces typed data; every command
//! is appended to the conversation log before its effect is attempted.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::anyhow;
use nori_pkl::Operation;
use tokio::sync::RwLock;
use tokio::sync::oneshot;

use super::PromptAdmission;
use crate::backend::AcpBackend;
use crate::transcript::TranscriptRecorder;

#[derive(Clone, Debug, serde::Serialize)]
pub struct ConversationProgramResult {
    pub program_id: String,
    pub applied: i64,
    pub results: Vec<serde_json::Value>,
}

pub(crate) async fn execute(
    source: &str,
    backend: &Arc<AcpBackend>,
    recorder_cell: &Arc<RwLock<Option<Arc<TranscriptRecorder>>>>,
) -> anyhow::Result<ConversationProgramResult> {
    execute_source(source, None, backend, recorder_cell).await
}

pub(crate) async fn execute_source(
    source: &str,
    path: Option<std::path::PathBuf>,
    backend: &Arc<AcpBackend>,
    recorder_cell: &Arc<RwLock<Option<Arc<TranscriptRecorder>>>>,
) -> anyhow::Result<ConversationProgramResult> {
    let recorder = recorder_cell
        .read()
        .await
        .clone()
        .ok_or_else(|| anyhow!("conversation programs require an event log"))?;
    let program_id = uuid::Uuid::new_v4().to_string();
    let mut expected_generation = backend.program_conversation_generation();
    recorder
        .record_command(
            "conversation-program",
            serde_json::json!({"program_id": program_id, "source": source, "path": path, "generation": expected_generation}).to_string(),
        )
        .await?;
    let outcome: anyhow::Result<ConversationProgramResult> = async {
        let first_id = recorder.conversation_next_id().await?;
        let properties = backend.program_machine_properties().await?;
        let source = source.to_owned();
        let commands = tokio::task::spawn_blocking(move || match path {
            Some(path) => nori_pkl::compile_source_at_path(&source, &path, first_id, properties),
            None => nori_pkl::compile_with_properties(&source, first_id, properties),
        }).await??;

        let mut results = Vec::new();
        let mut local_actors = HashMap::new();
        for command in &commands {
            let is_rewind = matches!(command.operation, Operation::Rewind { .. });
            // Rewind takes the write side itself; other effects retain the read
            // side from generation validation through their entire execution.
            let _admission = if is_rewind { None } else { Some(backend.program_effect_admission().await) };
            anyhow::ensure!(
                backend.program_conversation_generation() == expected_generation,
                "The conversation was rewound before this program could continue"
            );
            let payload = serde_json::to_string(&serde_json::json!({
                "program_id": program_id,
                "id": command.id,
                "operation": command.operation,
            }))?;
            recorder.record_command("pkl-operation", payload).await?;
            let outcome = dispatch(command.operation.clone(), backend, &recorder, &mut local_actors, expected_generation).await;
            if outcome.is_ok() && matches!(command.operation, Operation::Rewind { .. }) {
                expected_generation = expected_generation.checked_add(1)
                    .ok_or_else(|| anyhow!("conversation generation overflow"))?;
            }
            let receipt = match &outcome {
                Ok(result) => serde_json::json!({"program_id": program_id, "id": command.id, "success": true, "result": result}),
                Err(error) => serde_json::json!({"program_id": program_id, "id": command.id, "success": false, "error": error.to_string()}),
            };
            recorder.record_command("pkl-operation-result", receipt.to_string()).await?;
            if let Some(result) = outcome? {
                results.push(serde_json::json!({"id": command.id, "result": result}));
            }
        }
        Ok(ConversationProgramResult {
            program_id: program_id.clone(),
            applied: i64::try_from(commands.len())?,
            results,
        })
    }.await;
    let receipt = match &outcome {
        Ok(result) => {
            serde_json::json!({"program_id": program_id, "success": true, "applied": result.applied})
        }
        Err(error) => {
            serde_json::json!({"program_id": program_id, "success": false, "error": error.to_string()})
        }
    };
    recorder
        .record_command("conversation-program-result", receipt.to_string())
        .await?;
    outcome
}

async fn dispatch(
    operation: Operation,
    backend: &Arc<AcpBackend>,
    recorder: &TranscriptRecorder,
    local_actors: &mut HashMap<String, String>,
    expected_generation: i64,
) -> anyhow::Result<Option<String>> {
    match operation {
        Operation::Prompt { text } => {
            let (response_tx, response_rx) = oneshot::channel();
            backend
                .submit_prompt(
                    vec![nori_protocol::acp::v1::ContentBlock::Text(
                        nori_protocol::acp::v1::TextContent::new(text),
                    )],
                    None,
                    PromptAdmission::Queue,
                    response_tx,
                )
                .await;
            response_rx.await??;
            Ok(None)
        }
        Operation::Send { recipient, message } => {
            let session_id = match local_actors.get(&recipient) {
                Some(session_id) => session_id.clone(),
                None => recorder
                    .recorded_command_payload(&format!("pkl-actor-binding:{recipient}"))
                    .await
                    .and_then(|payload| serde_json::from_str::<String>(&payload).ok())
                    .unwrap_or_else(|| recipient.clone()),
            };
            recorder
                .record_command(
                    "pkl-actor-send",
                    serde_json::json!({
                        "actor": recipient, "session_id": session_id, "message": message,
                    })
                    .to_string(),
                )
                .await?;
            backend
                .send_subagent_message(session_id.into(), message)
                .await
                .map(|()| None)
        }
        Operation::Config { key, value } => {
            backend.set_config_option(key, value).await.map(|()| None)
        }
        Operation::Spawn { actor, agent } => {
            let session_id = backend
                .spawn_conversation_agent(actor.clone(), agent)
                .await?;
            recorder
                .record_command(
                    &format!("pkl-actor-binding:{actor}"),
                    serde_json::to_string(&session_id)?,
                )
                .await?;
            local_actors.insert(actor.clone(), session_id.clone());
            Ok(Some(
                serde_json::json!({"actor": actor, "session_id": session_id}).to_string(),
            ))
        }
        Operation::Fork { actor, start, end } => backend
            .fork_history(
                String::new(),
                Some((
                    if actor == "self" {
                        recorder.session_id().to_string()
                    } else {
                        actor
                    },
                    start,
                    end,
                )),
            )
            .await
            .map(|()| None),
        Operation::Rewind { head } => backend
            .rewind_program(recorder.session_id().to_string(), head, expected_generation)
            .await
            .map(|()| None),
        Operation::SelectContext { start, end } => {
            let records = recorder
                .projected_fork_records(recorder.session_id(), start, end)
                .await?;
            Ok(Some(crate::transcript::fork_context::context_from_records(
                records,
            )?))
        }
        Operation::Unsafe { program, args, cwd } => {
            let result = backend
                .execute_unsafe_program(&program, &args, cwd.as_deref())
                .await
                .map_err(|error| anyhow!("failed to start unsafe program {program}: {error}"))?;
            recorder
                .record_command("unsafe-result", result.to_string())
                .await?;
            if result.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
                return Err(anyhow!(
                    "unsafe program {program} failed: {}",
                    result["status"]
                ));
            }
            Ok(Some(result.to_string()))
        }
        Operation::ToolRequest { id, tool, input } => {
            let arguments = match serde_json::from_str(&input) {
                Ok(arguments) => arguments,
                Err(error) => {
                    let message = format!("invalid input for host tool {tool}: {error}");
                    recorder
                        .record_command(
                            "tool-result",
                            serde_json::json!({
                                "id": id, "tool": tool, "output": message, "is_error": true,
                            })
                            .to_string(),
                        )
                        .await?;
                    return Err(anyhow!(message));
                }
            };
            let result = match backend.execute_host_tool(&tool, arguments).await {
                Ok(result) => result,
                Err(error) => {
                    recorder
                        .record_command(
                            "tool-result",
                            serde_json::json!({
                                "id": id, "tool": tool, "output": error, "is_error": true,
                            })
                            .to_string(),
                        )
                        .await?;
                    return Err(anyhow!(error));
                }
            };
            recorder
                .record_command(
                    "tool-result",
                    serde_json::json!({
                        "id": id, "tool": tool, "output": result.text, "is_error": result.is_error,
                        "mcp_result": result.as_mcp_result(),
                    })
                    .to_string(),
                )
                .await?;
            if result.is_error {
                return Err(anyhow!(result.text));
            }
            Ok(Some(result.text))
        }
        Operation::ToolResult { id, output } => {
            recorder
                .record_command(
                    "tool-result",
                    serde_json::json!({"id": id, "output": output}).to_string(),
                )
                .await?;
            Ok(Some(output))
        }
    }
}
