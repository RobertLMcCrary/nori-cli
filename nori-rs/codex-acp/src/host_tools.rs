//! Nori's host tools, given to Codex as dynamic tools.
//!
//! When Nori runs this agent in its own process, a session request's `_meta` carries a token for
//! the session's tools (see `nori_host_tools`). They reach the model as Codex dynamic tools: the
//! specs go in at thread start, their instructions join the thread's instructions, and when the
//! model calls one, core emits `DynamicToolCallRequest` and waits for an
//! `Op::DynamicToolResponse`, which [`answer`] sends after running Nori's code directly.

use std::sync::Arc;

use codex_core_api::Instructions;
use codex_core_api::LoadInstructionsFuture;
use codex_core_api::LoadedUserInstructions;
use codex_core_api::ThreadInstructionsProvider;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::protocol::Op;
use nori_host_tools::HostTools;
use tracing::warn;

use crate::thread::CodexThreadImpl;

/// The tools as Codex dynamic tools, loaded up front so the model always sees them.
pub(crate) fn dynamic_tools(tools: &dyn HostTools) -> Vec<DynamicToolSpec> {
    tools
        .specs()
        .into_iter()
        .map(|spec| {
            DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: spec.name,
                description: spec.description,
                input_schema: spec.input_schema,
                defer_loading: false,
            })
        })
        .collect()
}

/// The tools' instructions, as the thread's own instructions.
pub(crate) struct HostInstructions(pub(crate) Arc<dyn HostTools>);

impl ThreadInstructionsProvider for HostInstructions {
    fn load_thread_instructions(&self) -> LoadInstructionsFuture<'_> {
        let text = self.0.instructions();
        Box::pin(async move {
            LoadedUserInstructions {
                instructions: (!text.trim().is_empty())
                    .then_some(Instructions { text, source: None }),
                warnings: Vec::new(),
            }
        })
    }
}

/// Run the host tool the model called and hand core its answer. Core waits for the answer, so
/// one is always sent: an error when the tool fails, or when this session has no host tools
/// (a thread saved with them, resumed by a client that does not give them).
pub(crate) fn answer(
    tools: Option<Arc<dyn HostTools>>,
    thread: Arc<dyn CodexThreadImpl>,
    call_id: String,
    tool: String,
    arguments: serde_json::Value,
) {
    tokio::spawn(async move {
        let output = match tools {
            Some(tools) => tools.call(&tool, arguments).await,
            None => nori_host_tools::HostToolOutput::error(format!(
                "`{tool}` is not available in this session"
            )),
        };
        let op = Op::DynamicToolResponse {
            id: call_id,
            response: DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: output.text,
                }],
                success: !output.is_error,
            },
        };
        if let Err(error) = thread.submit(op).await {
            warn!("Could not answer the call to host tool {tool}: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use nori_host_tools::HostToolFuture;
    use nori_host_tools::HostToolOutput;
    use nori_host_tools::HostToolSpec;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    struct Tools;

    impl HostTools for Tools {
        fn instructions(&self) -> String {
            "Commit in small batches.".to_owned()
        }

        fn specs(&self) -> Vec<HostToolSpec> {
            vec![HostToolSpec {
                name: "git_commit".to_owned(),
                description: "Commit some paths.".to_owned(),
                input_schema: json!({"type": "object", "required": ["paths"]}),
                annotations: Some(json!({"destructiveHint": false})),
                meta: None,
            }]
        }

        fn call(&self, _name: &str, _arguments: serde_json::Value) -> HostToolFuture {
            Box::pin(async { HostToolOutput::error("unused") })
        }
    }

    #[test]
    fn specs_become_function_tools_the_model_always_sees() {
        let specs = dynamic_tools(&Tools);
        let [DynamicToolSpec::Function(spec)] = specs.as_slice() else {
            panic!("one function tool: {specs:?}");
        };
        assert_eq!(
            spec,
            &DynamicToolFunctionSpec {
                name: "git_commit".to_owned(),
                description: "Commit some paths.".to_owned(),
                input_schema: json!({"type": "object", "required": ["paths"]}),
                defer_loading: false,
            }
        );
    }

    #[tokio::test]
    async fn instructions_reach_the_thread() {
        let loaded = HostInstructions(Arc::new(Tools))
            .load_thread_instructions()
            .await;
        assert_eq!(
            loaded.instructions.map(|instructions| instructions.text),
            Some("Commit in small batches.".to_owned())
        );
    }
}
