//! Scripted calls to the client's `nori-client` MCP server, so tests can drive delegation.
//!
//! `MOCK_AGENT_MCP_CALLS_<AGENT>` (or `MOCK_AGENT_MCP_CALLS`) holds a JSON array of
//! `{"tool": ..., "arguments": {...}}`. On a prompt containing `MOCK_AGENT_MCP_TRIGGER` (default
//! `DELEGATE`), each call is made in order and shown as a tool call; a string argument
//! `"$task_id"` is replaced by the `task_id` the previous call returned. The server is reached
//! over HTTP when the client offers it so, else over stdio by running the command it gave.

use nori_protocol::acp::v1 as acp;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

/// One scripted call.
pub(crate) struct Call {
    pub(crate) tool: String,
    pub(crate) arguments: Value,
}

/// The calls scripted for this agent, when the prompt asks for them.
pub(crate) fn scripted(prompt_text: &str) -> Vec<Call> {
    let trigger = std::env::var("MOCK_AGENT_MCP_TRIGGER").unwrap_or_else(|_| "DELEGATE".to_owned());
    if !prompt_text.contains(&trigger) {
        return Vec::new();
    }
    let model_name = std::env::var("MOCK_AGENT_MODEL_NAME").unwrap_or_default();
    let specific = format!(
        "MOCK_AGENT_MCP_CALLS_{}",
        model_name.replace('-', "_").to_uppercase()
    );
    let Ok(script) = std::env::var(&specific).or_else(|_| std::env::var("MOCK_AGENT_MCP_CALLS"))
    else {
        return Vec::new();
    };
    let Ok(Value::Array(calls)) = serde_json::from_str::<Value>(&script) else {
        eprintln!("Mock agent: {specific} is not a JSON array");
        return Vec::new();
    };
    calls
        .into_iter()
        .filter_map(|call| {
            Some(Call {
                tool: call.get("tool")?.as_str()?.to_owned(),
                arguments: call.get("arguments").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// Replace `"$task_id"` strings with `task_id`.
pub(crate) fn substitute(arguments: &mut Value, task_id: Option<&str>) {
    match arguments {
        Value::String(text) if text == "$task_id" => {
            if let Some(task_id) = task_id {
                *text = task_id.to_owned();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| substitute(item, task_id)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|value| substitute(value, task_id)),
        _ => {}
    }
}

/// A connection to the `nori-client` server the client advertised.
pub(crate) enum Server {
    Http {
        url: String,
        authorization: Option<String>,
    },
    Stdio {
        command: std::path::PathBuf,
        args: Vec<String>,
    },
}

impl Server {
    pub(crate) fn find(servers: &[acp::McpServer]) -> Option<Self> {
        servers.iter().find_map(|server| match server {
            acp::McpServer::Http(http) if http.name == "nori-client" => Some(Self::Http {
                url: http.url.clone(),
                authorization: http
                    .headers
                    .iter()
                    .find(|header| header.name == "Authorization")
                    .map(|header| header.value.clone()),
            }),
            acp::McpServer::Stdio(stdio) if stdio.name == "nori-client" => Some(Self::Stdio {
                command: stdio.command.clone(),
                args: stdio.args.clone(),
            }),
            _ => None,
        })
    }

    /// Call `tool`, returning its text and whether it reported an error.
    pub(crate) async fn call(
        &self,
        tool: &str,
        arguments: &Value,
    ) -> Result<(String, bool), String> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        });
        let response = match self {
            Self::Http { url, authorization } => {
                http_call(url, authorization.as_deref(), &request).await?
            }
            Self::Stdio { command, args } => stdio_call(command, args, &request).await?,
        };
        let result = response
            .get("result")
            .ok_or_else(|| format!("tools/call failed: {response}"))?;
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|content| content.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok((text, is_error))
    }
}

async fn http_call(
    url: &str,
    authorization: Option<&str>,
    request: &Value,
) -> Result<Value, String> {
    let without_scheme = url
        .strip_prefix("http://")
        .ok_or("nori-client URL should use plain HTTP")?;
    let (host_port, path) = without_scheme
        .split_once('/')
        .ok_or("nori-client URL should include a path")?;
    let body = request.to_string();
    let authorization = authorization
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let http = format!(
        "POST /{path} HTTP/1.1\r\nHost: {host_port}\r\nAccept: application/json, text/event-stream\r\n\
         Content-Type: application/json\r\n{authorization}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect(host_port)
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(http.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .map_err(|error| error.to_string())?;
    // A streamable-HTTP answer is either a JSON body or server-sent events carrying it.
    response
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .chain(response.split("\r\n\r\n").nth(1))
        .filter_map(|candidate| serde_json::from_str::<Value>(candidate.trim()).ok())
        .find(|message| message.get("id").is_some())
        .ok_or_else(|| format!("no JSON-RPC answer in: {response}"))
}

async fn stdio_call(
    command: &std::path::Path,
    args: &[String],
    request: &Value,
) -> Result<Value, String> {
    let mut child = tokio::process::Command::new(command)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", command.display()))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().ok_or("no stdout")?).lines();
    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "mock-acp-agent", "version": "0" }
        }
    });
    let initialized =
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    for message in [&initialize, &initialized, request] {
        stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
            .map_err(|error| error.to_string())?;
    }
    let wanted = request.get("id").cloned();
    while let Some(line) = stdout
        .next_line()
        .await
        .map_err(|error| error.to_string())?
    {
        if let Ok(message) = serde_json::from_str::<Value>(&line)
            && message.get("id") == wanted.as_ref()
        {
            return Ok(message);
        }
    }
    Err("the stdio server closed before answering".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_are_substituted_anywhere_in_the_arguments() {
        let mut arguments =
            serde_json::json!({ "task_id": "$task_id", "task_ids": ["$task_id", "x"] });
        substitute(&mut arguments, Some("task-3"));
        assert_eq!(
            arguments,
            serde_json::json!({ "task_id": "task-3", "task_ids": ["task-3", "x"] })
        );
    }
}
