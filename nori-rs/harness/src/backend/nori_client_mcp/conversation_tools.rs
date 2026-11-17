//! The native Pkl conversation-program tool.
use super::*;

const PROGRAM_HELP: &str = "Pkl syntax: commands = List(new Dynamic { op = \"spawn\"; actor = \"reviewer\"; agent = \"claude\" }, new Dynamic { op = \"send\"; recipient = \"reviewer\"; message = \"Review this\" }). Valid operations and fields: prompt(text); spawn(actor, agent); send(recipient, message); config(key, value); tool-request(id, tool, input: JSON string); tool-result(id, output); fork(actor, start, end); rewind(head: integer or null); select-context(start, end); unsafe(program, optional args and cwd). There is no wait operation. send queues child work and returns without waiting; do not invent a wait or prompt field. Pkl actor sends do not return a task ID or wait for a reply. Use delegate_task and task_wait for a separate tracked delegation when you need a result. prompt targets the current session only.";

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct ConversationProgramRequest {
    /// Pkl module with ordered commands and an optional explicit unsafe block.
    source: String,
}

#[tool_router(router = conversation_tool_router, vis = "pub(crate)")]
impl NoriClientService {
    #[tool(
        description = "Evaluate and apply a Pkl conversation program. Example: commands = List(new Dynamic { op = \"spawn\"; actor = \"reviewer\"; agent = \"claude\" }, new Dynamic { op = \"send\"; recipient = \"reviewer\"; message = \"Review this\" }). Each command uses op and exactly these fields: prompt(text); spawn(actor, agent); send(recipient, message); config(key, value); tool-request(id, tool, input as JSON string); tool-result(id, output); fork(actor, start, end); rewind(head as integer or null); select-context(start, end); unsafe(program, optional args and cwd). There is no wait operation. send queues child work and returns without waiting; Pkl actor sends do not return a task ID or wait for a reply. prompt targets the current session only; do not use it to address an actor. Use delegate_task and task_wait for a separate tracked delegation when you need a result. Commands and results are recorded before effects; replay never executes them."
    )]
    pub(crate) async fn conversation(
        &self,
        Parameters(request): Parameters<ConversationProgramRequest>,
    ) -> Result<CallToolResult, McpError> {
        let result = self
            .shared
            .orchestrator
            .execute_conversation_program(request.source)
            .await
            .map_err(|error| McpError::internal_error(format!("{error}\n{PROGRAM_HELP}"), None))?;
        Ok(tool_success(serde_json::to_value(result).map_err(
            |error| McpError::internal_error(error.to_string(), None),
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_definition_and_errors_explain_real_program_commands() {
        let definition = NoriClientService::all_tools()
            .into_iter()
            .find(|tool| tool.name == "conversation")
            .unwrap();
        let description = definition.description.unwrap();
        for term in [
            "spawn(actor, agent)",
            "send(recipient, message)",
            "prompt(text)",
            "There is no wait operation",
        ] {
            assert!(description.contains(term), "{term}");
            assert!(PROGRAM_HELP.contains(term), "{term}");
        }
    }
}
