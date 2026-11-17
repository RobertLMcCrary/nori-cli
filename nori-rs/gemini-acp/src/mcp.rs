use acp::schema::McpServer;
use agent_client_protocol as acp;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

pub(crate) fn translate(servers: Vec<McpServer>) -> acp::Result<Map<String, Value>> {
    let mut result = Map::new();
    for server in servers {
        let (name, config) = match server {
            McpServer::Stdio(server) => {
                let env: Map<String, Value> = server
                    .env
                    .into_iter()
                    .map(|entry| (entry.name, entry.value.into()))
                    .collect();
                (
                    server.name,
                    json!({"command":server.command, "args":server.args, "env":env}),
                )
            }
            McpServer::Http(server) => {
                let headers: Map<String, Value> = server
                    .headers
                    .into_iter()
                    .map(|entry| (entry.name, entry.value.into()))
                    .collect();
                (
                    server.name,
                    json!({"httpUrl":server.url, "headers":headers}),
                )
            }
            McpServer::Sse(server) => {
                let headers: Map<String, Value> = server
                    .headers
                    .into_iter()
                    .map(|entry| (entry.name, entry.value.into()))
                    .collect();
                (server.name, json!({"url":server.url, "headers":headers}))
            }
            _ => return Err(acp::Error::invalid_params().data("Unsupported MCP server transport")),
        };
        if result.insert(name, config).is_some() {
            return Err(acp::Error::invalid_params().data("Duplicate MCP server name"));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    #[test]
    fn translates_mcp_transports() {
        let input = json!([
            {"name":"local","command":"server","args":["--stdio"],"env":[{"name":"KEY","value":"value"}]},
            {"type":"http","name":"web","url":"https://example.invalid/mcp","headers":[{"name":"X-Key","value":"value"}]},
            {"type":"sse","name":"events","url":"https://example.invalid/events","headers":[]}
        ]);
        let servers = serde_json::from_value(input).unwrap();
        assert_eq!(
            Value::Object(translate(servers).unwrap()),
            json!({
                "local":{"command":"server","args":["--stdio"],"env":{"KEY":"value"}},
                "web":{"httpUrl":"https://example.invalid/mcp","headers":{"X-Key":"value"}},
                "events":{"url":"https://example.invalid/events","headers":{}}
            })
        );
    }
}
