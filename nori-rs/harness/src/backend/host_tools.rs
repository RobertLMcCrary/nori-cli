//! Nori's own tools handed to an agent built into this process, with no MCP server between them.
//!
//! The tools are the `nori-client` server's `#[tool]` methods. Here they are listed from the same
//! router and called directly, so a built-in agent and an MCP client see the same tools, schemas
//! and replies. How a built-in agent finds them is described in [`nori_host_tools`].

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use nori_host_tools::HostToolFuture;
use nori_host_tools::HostToolOutput;
use nori_host_tools::HostToolSpec;
use nori_host_tools::HostTools;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::nori_client_mcp::NoriClientService;
use super::nori_client_mcp::NoriClientShared;
use crate::connection::acp_connection::AcpConnection;

/// Whether `connection` is to a built-in agent that takes host tools. Only an agent in this
/// process can resolve a registration token, so an agent elsewhere never gets one.
pub(super) fn accepted_by(connection: &AcpConnection) -> bool {
    connection.is_in_process() && nori_host_tools::advertised(connection.initialize_meta())
}

/// Publish what every session's tools look like, once, so a built-in agent can prepare sessions
/// before any exists (see [`nori_host_tools::Catalog`]).
pub(crate) fn publish_catalog() {
    static PUBLISHED: std::sync::Once = std::sync::Once::new();
    PUBLISHED.call_once(|| {
        nori_host_tools::publish_catalog(nori_host_tools::Catalog {
            specs: specs(),
            instructions: super::nori_client_mcp::tool_instructions(),
        });
    });
}

/// Register `shared`'s tools and have every later session request on `connection` carry them.
/// The tools are usable as soon as the agent has the session, so they count as connected.
pub(super) fn hand_to(
    connection: &AcpConnection,
    shared: NoriClientShared,
    connected: &AtomicBool,
) -> nori_host_tools::Registration {
    publish_catalog();
    let registration = nori_host_tools::register(Arc::new(NoriHostTools {
        service: NoriClientService::new(shared),
    }));
    connection.set_session_meta(Some(registration.session_meta()));
    connected.store(true, Ordering::Relaxed);
    registration
}

struct NoriHostTools {
    service: NoriClientService,
}

impl HostTools for NoriHostTools {
    fn instructions(&self) -> String {
        super::nori_client_mcp::tool_instructions()
    }

    fn specs(&self) -> Vec<HostToolSpec> {
        specs()
    }

    fn call(&self, name: &str, arguments: Value) -> HostToolFuture {
        let service = self.service.clone();
        let name = name.to_owned();
        Box::pin(async move {
            match dispatch(&service, &name, arguments).await {
                Ok(result) => HostToolOutput {
                    text: result
                        .content
                        .iter()
                        .filter_map(|content| content.as_text())
                        .map(|text| text.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    is_error: result.is_error.unwrap_or(false),
                },
                Err(message) => HostToolOutput::error(message),
            }
        })
    }
}

/// Every tool as a host tool spec, in a stable order: that keeps the agent's tool list, and so
/// its prompt cache, the same across sessions.
fn specs() -> Vec<HostToolSpec> {
    let mut tools = NoriClientService::all_tools();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
        .into_iter()
        .map(|tool| HostToolSpec {
            name: tool.name.to_string(),
            description: tool.description.map(String::from).unwrap_or_default(),
            input_schema: Value::Object((*tool.input_schema).clone()),
            annotations: tool
                .annotations
                .and_then(|annotations| serde_json::to_value(annotations).ok()),
            meta: tool.meta.map(|meta| meta.0),
        })
        .collect()
}

/// Call the tool `name` as the MCP server would. Every tool the router lists has an arm here;
/// a test holds the two together.
async fn dispatch(
    service: &NoriClientService,
    name: &str,
    arguments: Value,
) -> Result<CallToolResult, String> {
    let result = match name {
        "get_goal" => {
            no_arguments(name, &arguments)?;
            service.get_goal().await
        }
        "create_goal" => service.create_goal(parameters(arguments)?).await,
        "update_goal" => service.update_goal(parameters(arguments)?).await,
        "git_status" => {
            no_arguments(name, &arguments)?;
            service.git_status().await
        }
        "git_diff" => service.git_diff(parameters(arguments)?).await,
        "git_log" => service.git_log(parameters(arguments)?).await,
        "git_commit" => service.git_commit(parameters(arguments)?).await,
        "agent_capabilities" => {
            no_arguments(name, &arguments)?;
            service.agent_capabilities().await
        }
        "delegate_task" => service.delegate_task(parameters(arguments)?).await,
        "task_status" => service.task_status(parameters(arguments)?).await,
        "task_wait" => service.task_wait(parameters(arguments)?).await,
        "task_send" => service.task_send(parameters(arguments)?).await,
        "task_cancel" => service.task_cancel(parameters(arguments)?).await,
        "task_list" => {
            no_arguments(name, &arguments)?;
            service.task_list().await
        }
        _ => return Err(format!("there is no Nori tool named {name}")),
    };
    result.map_err(|error| error.message.into_owned())
}

/// A tool's arguments, as rmcp would parse them. A model that sends no arguments at all means an
/// empty object.
fn parameters<T: DeserializeOwned>(arguments: Value) -> Result<Parameters<T>, String> {
    let arguments = if arguments.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        arguments
    };
    serde_json::from_value(arguments)
        .map(Parameters)
        .map_err(|error| format!("invalid arguments: {error}"))
}

fn no_arguments(name: &str, arguments: &Value) -> Result<(), String> {
    match arguments {
        Value::Null => Ok(()),
        Value::Object(map) if map.is_empty() => Ok(()),
        _ => Err(format!("invalid arguments: {name} takes none")),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::Mutex;
    use tokio::sync::mpsc;

    use super::*;
    use crate::backend::orchestrator::Orchestrator;
    use crate::backend::orchestrator::Seed;
    use crate::backend::thread_goal::ThreadGoalState;

    fn tools(cwd: Option<std::path::PathBuf>) -> NoriHostTools {
        let (events, _events_rx) = mpsc::channel(8);
        let (orchestrator, _signals) = Orchestrator::new(
            Seed::root(
                Arc::new(crate::config::NoriConfig::default()),
                "test".to_owned(),
            ),
            events.clone(),
        );
        let shared = NoriClientShared::new(
            Arc::new(Mutex::new(ThreadGoalState::default())),
            events,
            Arc::new(AtomicBool::new(false)),
            crate::normalized::AgentCapabilitiesView {
                http_mcp: false,
                load_session: false,
                session_list: false,
                session_resume: false,
                session_close: false,
                session_fork: false,
            },
            true,
            false,
            orchestrator,
        );
        NoriHostTools {
            service: NoriClientService::new(match cwd {
                Some(cwd) => shared.with_cwd(cwd),
                None => shared,
            }),
        }
    }

    #[test]
    fn the_tools_are_the_mcp_servers_in_a_stable_order() {
        let names: Vec<String> = tools(None)
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "agent_capabilities",
                "create_goal",
                "delegate_task",
                "get_goal",
                "git_commit",
                "git_diff",
                "git_log",
                "git_status",
                "task_cancel",
                "task_list",
                "task_send",
                "task_status",
                "task_wait",
                "update_goal",
            ]
        );
    }

    #[test]
    fn every_spec_has_an_object_schema_and_delegation_stays_loaded() {
        for spec in tools(None).specs() {
            assert_eq!(spec.input_schema["type"], "object", "{}", spec.name);
            assert!(!spec.description.is_empty(), "{}", spec.name);
        }
        let delegate = tools(None)
            .specs()
            .into_iter()
            .find(|spec| spec.name == "delegate_task")
            .expect("delegate_task");
        assert_eq!(
            delegate
                .meta
                .and_then(|meta| meta.get("anthropic/alwaysLoad").cloned()),
            Some(json!(true))
        );
    }

    /// Every listed tool reaches its own arm: given an argument no tool takes, each one rejects
    /// it as an argument error rather than as an unknown tool, and nothing runs.
    #[tokio::test]
    async fn every_listed_tool_is_dispatched() {
        let tools = tools(None);
        for spec in tools.specs() {
            let output = tools
                .call(&spec.name, json!({"no_such_argument": true}))
                .await;
            assert!(output.is_error, "{}", spec.name);
            assert!(
                output.text.starts_with("invalid arguments"),
                "{}: {}",
                spec.name,
                output.text
            );
        }
        let unknown = tools.call("rm_rf", json!({})).await;
        assert_eq!(
            unknown,
            HostToolOutput::error("there is no Nori tool named rm_rf")
        );
    }

    #[tokio::test]
    async fn a_goal_round_trips_through_direct_calls() {
        let tools = tools(None);
        assert_eq!(
            tools.call("get_goal", Value::Null).await,
            HostToolOutput {
                text: json!({"goal": null}).to_string(),
                is_error: false,
            }
        );
        let created = tools
            .call("create_goal", json!({"objective": "ship host tools"}))
            .await;
        assert!(!created.is_error, "{}", created.text);
        let goal: Value = serde_json::from_str(&created.text).expect("json");
        assert_eq!(goal["goal"]["objective"], "ship host tools");
        assert_eq!(goal["goal"]["status"], "active");

        let again = tools
            .call("create_goal", json!({"objective": "another"}))
            .await;
        assert!(again.is_error);
    }

    #[tokio::test]
    async fn git_status_without_a_working_directory_is_an_error_not_a_panic() {
        let output = tools(None).call("git_status", json!({})).await;
        assert_eq!(
            output,
            HostToolOutput::error("this session has no working directory")
        );
    }
}
