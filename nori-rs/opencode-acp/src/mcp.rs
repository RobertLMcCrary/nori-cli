//! Translate ACP's MCP server configuration to OpenCode v2 configuration.

use acp::schema::McpServer;
use agent_client_protocol as acp;
use reqwest::Method;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::OpenCodeServer;

pub(crate) async fn register(server: &OpenCodeServer, servers: Vec<McpServer>) -> acp::Result<()> {
    for definition in servers {
        let (name, config) = match definition {
            McpServer::Stdio(definition) => {
                let mut command = vec![
                    definition
                        .command
                        .to_str()
                        .ok_or_else(acp::Error::invalid_params)?
                        .to_owned(),
                ];
                command.extend(definition.args);
                let environment: Map<String, Value> = definition
                    .env
                    .into_iter()
                    .map(|entry| (entry.name, entry.value.into()))
                    .collect();
                (
                    definition.name,
                    json!({"type":"local","command":command,"environment":environment}),
                )
            }
            McpServer::Http(definition) => {
                let headers: Map<String, Value> = definition
                    .headers
                    .into_iter()
                    .map(|entry| (entry.name, entry.value.into()))
                    .collect();
                (
                    definition.name,
                    json!({"type":"remote","url":definition.url,"headers":headers,"oauth":false}),
                )
            }
            _ => {
                return Err(acp::Error::invalid_params()
                    .data("OpenCode supports stdio and HTTP MCP servers"));
            }
        };
        server
            .request(
                Method::PUT,
                &["experimental", "mcp", &name],
                Some(&json!({"config":config})),
            )
            .await
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    }
    Ok(())
}
