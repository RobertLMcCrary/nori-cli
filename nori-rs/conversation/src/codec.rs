use crate::Event;
use crate::Record;
use crate::SliceRef;
use serde_json::Value;
use serde_json::json;
use std::io;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid conversation record")
}

// Each property is a complete Pkl object with a typed operation name and a
// JSON payload encoded as a Pkl string. The double encoding ensures all JSON
// control escapes remain ordinary literal backslashes in Pkl.
pub(crate) fn encode(record: &Record) -> io::Result<String> {
    let (kind, data) = match &record.event {
        Event::Message { role, content } => ("message", json!({"role": role, "content": content})),
        Event::ToolCall { id, name, input } => {
            ("tool-call", json!({"id": id, "name": name, "input": input}))
        }
        Event::ToolResult { id, output } => ("tool-result", json!({"id": id, "output": output})),
        Event::Rewind { head } => ("rewind", json!({"head": head})),
        Event::Fork { source } => ("fork", json!({"start": source.start, "end": source.end})),
        Event::Operation { name, args } => ("operation", json!({"name": name, "args": args})),
        Event::Command { name, payload } => ("command", json!({"name": name, "payload": payload})),
        Event::SessionEvent { payload } => ("session-event", json!({"payload": payload})),
        Event::Unknown { form } => return Ok(format!("{form}\n")),
    };
    let payload = serde_json::to_string(&data.to_string()).map_err(io::Error::other)?;
    let command = record
        .command_id
        .as_ref()
        .map_or_else(String::new, |id| format!("; commandId = \"{id}\""));
    Ok(format!(
        "event_{} = new {{ kind = \"{kind}\"; data = {payload}{command} }}\n",
        record.id
    ))
}

pub(crate) fn decode(line: &str) -> io::Result<Record> {
    let (id, body) = line
        .strip_prefix("event_")
        .and_then(|s| s.split_once(" = new { kind = "))
        .ok_or_else(invalid)?;
    let id: i64 = id.parse().map_err(|_| invalid())?;
    if id <= 0 {
        return Err(invalid());
    }
    let (kind, data) = body
        .strip_prefix('"')
        .and_then(|s| s.split_once("\"; data = "))
        .ok_or_else(invalid)?;
    let data = data.strip_suffix(" }").ok_or_else(invalid)?;
    let (data, command_id) = match data.rsplit_once("; commandId = \"") {
        Some((data, id)) => (
            data,
            Some(serde_json::from_str(&format!("\"{id}")).map_err(|_| invalid())?),
        ),
        None => (data, None),
    };
    let event = match kind {
        "message" | "tool-call" | "tool-result" | "rewind" | "fork" | "operation" | "command"
        | "session-event" => {
            let payload: String = serde_json::from_str(data).map_err(|_| invalid())?;
            let data: Value = serde_json::from_str(&payload).map_err(|_| invalid())?;
            let string = |key: &str| {
                data.get(key)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(invalid)
            };
            let integer = |key: &str| data.get(key).and_then(Value::as_i64).ok_or_else(invalid);
            match kind {
                "message" => Event::Message {
                    role: string("role")?,
                    content: string("content")?,
                },
                "tool-call" => Event::ToolCall {
                    id: string("id")?,
                    name: string("name")?,
                    input: string("input")?,
                },
                "tool-result" => Event::ToolResult {
                    id: string("id")?,
                    output: string("output")?,
                },
                "rewind" => Event::Rewind {
                    head: match data.get("head") {
                        Some(Value::Null) => None,
                        Some(value) => Some(value.as_i64().ok_or_else(invalid)?),
                        None => return Err(invalid()),
                    },
                },
                "fork" => Event::Fork {
                    source: SliceRef {
                        start: integer("start")?,
                        end: integer("end")?,
                    },
                },
                "operation" => Event::Operation {
                    name: string("name")?,
                    args: string("args")?,
                },
                "command" => Event::Command {
                    name: string("name")?,
                    payload: string("payload")?,
                },
                "session-event" => Event::SessionEvent {
                    payload: string("payload")?,
                },
                _ => unreachable!(),
            }
        }
        _ => Event::Unknown {
            form: line.to_owned(),
        },
    };
    Ok(Record {
        id,
        command_id,
        event,
    })
}
