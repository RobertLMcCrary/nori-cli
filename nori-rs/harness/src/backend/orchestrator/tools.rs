//! The task tools of the `nori-client` MCP server.

use std::sync::Arc;
use std::time::Duration;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::schemars;
use rmcp::tool;
use rmcp::tool_router;
use serde::Deserialize;
use serde::Serialize;

use super::launch::Delegation;
use super::launch::Isolation;
use super::launch::Mode;
use super::launch::Role;
use super::task::TaskId;
use crate::backend::nori_client_mcp::NoriClientService;

/// What the server tells agents about delegation, after its other instructions.
pub(crate) const INSTRUCTIONS: &str = "Delegated tasks: `delegate_task` runs a task on a child agent, which may be a different \
     agent and model than yours (see `agent_capabilities`). Delegate independent work, then keep \
     working or end your turn: each result is delivered to you as a message when its task \
     finishes, so never poll. Give each child everything it needs in `task`; it sees none of \
     your conversation. Tasks end with this session, and the user stopping you stops them.";

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct DelegateTaskRequest {
    /// Required. The whole task. The child sees none of your conversation, so include every
    /// fact, file path, and constraint it needs, and say what its final message must contain.
    /// Never include secrets.
    task: String,
    /// The agent to run the child on: an `agent` value from `agent_capabilities`. Defaults to
    /// your own agent.
    #[serde(default)]
    agent: Option<String>,
    /// The child's model: a model value or name its agent offers. Defaults to the agent's own.
    #[serde(default)]
    model: Option<String>,
    /// A short title shown to the user, such as "Review the parser changes".
    #[serde(default)]
    title: Option<String>,
    /// The kind of work, which frames the child's instructions: `implementation`, `research`,
    /// `review`, `design`, `test`, or `general`. Tell a research or review child in `task` not to
    /// change files if it must not.
    #[serde(default)]
    role: Option<Role>,
    /// `shared` (default): the child works in your working directory. `worktree`: it works in a
    /// new git worktree at your current commit (uncommitted edits are not in it; it starts on a
    /// detached HEAD, so ask it to commit or branch and report where), for changes that must not
    /// collide with yours.
    #[serde(default)]
    isolation: Option<Isolation>,
    /// `background` (default): return at once; the result is delivered to you as a message when
    /// the task finishes. `wait`: block until it finishes or `timeout_seconds` pass.
    #[serde(default)]
    mode: Option<Mode>,
    /// With `mode: wait`, seconds to block: default 45, at most 50. A timeout does not cancel the
    /// task; its result is delivered to you when it finishes.
    #[serde(default)]
    timeout_seconds: Option<i64>,
    /// An idempotency key: delegating again with the same key returns the first task instead of
    /// starting another.
    #[serde(default)]
    client_request_id: Option<String>,
    /// Other settings for the child's agent, by setting and value, from the `settings` that
    /// `agent_capabilities` lists: for example `{"thought_level": "high"}`.
    #[serde(default)]
    options: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskIdRequest {
    /// Required. The `task_id` that `delegate_task` or `task_send` returned.
    task_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskWaitRequest {
    /// Required. The tasks to wait for.
    task_ids: Vec<String>,
    /// Seconds to block: default 45, at most 50. A timeout cancels nothing.
    #[serde(default)]
    timeout_seconds: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskSendRequest {
    /// Required. A finished task whose child gets the message.
    task_id: String,
    /// Required. The follow-up. The child keeps its whole conversation, so this can refer to it.
    message: String,
    /// `background` (default) or `wait`, as for `delegate_task`.
    #[serde(default)]
    mode: Option<Mode>,
    /// With `mode: wait`, seconds to block: default 45, at most 50.
    #[serde(default)]
    timeout_seconds: Option<i64>,
}

#[tool_router(router = task_tool_router, vis = "pub(crate)")]
impl NoriClientService {
    #[tool(
        description = "List the agents you can delegate tasks to, with the models and other settings (such as reasoning effort) each offers, and whether you can delegate now (depth and running-task limits).",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false),
        meta = always_load()
    )]
    async fn agent_capabilities(&self) -> Result<CallToolResult, McpError> {
        Ok(json(&self.orchestrator().inner().capabilities()))
    }

    #[tool(
        description = "Start a task on a child agent, which may run on a different agent and model than yours (see agent_capabilities). The child gets only `task`, none of your conversation, and cannot ask you questions. Returns at once with a `task_id`; the result is delivered to you as a message when the task finishes, so do not poll, and use `mode: wait` only when this turn cannot go on without it. With `isolation: shared` children edit your files directly: give parallel children disjoint files, or use `isolation: worktree`. Keep the `task_id` for task_send, task_status, and task_cancel. No secrets in `task`.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true),
        meta = always_load()
    )]
    async fn delegate_task(
        &self,
        Parameters(request): Parameters<DelegateTaskRequest>,
    ) -> Result<CallToolResult, McpError> {
        let delegation = Delegation {
            task: request.task,
            agent: request.agent,
            model: request.model,
            title: request.title,
            role: request.role,
            isolation: request.isolation.unwrap_or_default(),
            mode: request.mode.unwrap_or_default(),
            timeout: seconds(request.timeout_seconds),
            client_request_id: request.client_request_id,
            options: request.options.unwrap_or_default().into_iter().collect(),
        };
        let inner = Arc::clone(self.orchestrator().inner());
        Ok(outcome(
            detached(async move { inner.delegate(delegation).await }).await,
        ))
    }

    #[tool(
        description = "Read one delegated task: its status; while it runs, what its child is doing now (`activity`, `tool_calls`); the files its edit tools changed (not those its shell commands did); and once it has finished, the child's final message. Reading a finished task delivers its result, so it will not also arrive as a message.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false),
        meta = always_load()
    )]
    async fn task_status(
        &self,
        Parameters(TaskIdRequest { task_id }): Parameters<TaskIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(outcome(
            self.orchestrator()
                .inner()
                .status(&TaskId::parse(&task_id), true)
                .await,
        ))
    }

    #[tool(
        description = "Block until every listed task has finished, one of them is waiting for the user's approval, or the timeout passes, and return them. Only wait when you cannot continue without the results; finished results are delivered to you as messages anyway.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false),
        meta = always_load()
    )]
    async fn task_wait(
        &self,
        Parameters(request): Parameters<TaskWaitRequest>,
    ) -> Result<CallToolResult, McpError> {
        let ids: Vec<TaskId> = request
            .task_ids
            .iter()
            .map(|id| TaskId::parse(id))
            .collect();
        let timeout = seconds(request.timeout_seconds).unwrap_or(super::DEFAULT_WAIT);
        Ok(outcome(
            self.orchestrator()
                .inner()
                .wait(&ids, timeout)
                .await
                .map(|tasks| TaskList { tasks }),
        ))
    }

    #[tool(
        description = "Send a follow-up to the child that ran a finished task. The child keeps its whole conversation. Starts a new task on that child and returns its `task_id`; its result is delivered like any other.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true),
        meta = always_load()
    )]
    async fn task_send(
        &self,
        Parameters(request): Parameters<TaskSendRequest>,
    ) -> Result<CallToolResult, McpError> {
        let inner = Arc::clone(self.orchestrator().inner());
        Ok(outcome(
            detached(async move {
                inner
                    .send(
                        &TaskId::parse(&request.task_id),
                        request.message,
                        request.mode.unwrap_or_default(),
                        seconds(request.timeout_seconds),
                    )
                    .await
            })
            .await,
        ))
    }

    #[tool(
        description = "Stop a delegated task, and the tasks it delegated in turn. A task that has already finished keeps its result. A cancelled task's result is not delivered.",
        annotations(
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        ),
        meta = always_load()
    )]
    async fn task_cancel(
        &self,
        Parameters(TaskIdRequest { task_id }): Parameters<TaskIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let inner = Arc::clone(self.orchestrator().inner());
        Ok(outcome(
            detached(async move { inner.cancel(&TaskId::parse(&task_id)).await }).await,
        ))
    }

    #[tool(
        description = "List every task you delegated in this session with its status.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false),
        meta = always_load()
    )]
    async fn task_list(&self) -> Result<CallToolResult, McpError> {
        Ok(json(&TaskList {
            tasks: self.orchestrator().inner().list().await,
        }))
    }
}

#[derive(Serialize)]
struct TaskList {
    tasks: Vec<super::task::TaskSnapshot>,
}

/// Asks Claude Code to keep a tool loaded rather than behind its tool search, so the agent sees
/// that it can delegate.
fn always_load() -> rmcp::model::Meta {
    let mut meta = rmcp::model::Meta::new();
    meta.0.insert(
        "anthropic/alwaysLoad".to_owned(),
        serde_json::Value::Bool(true),
    );
    meta
}

/// Run a call that changes tasks on its own task, so a client that gives up on the call cannot
/// stop it halfway: the task it starts or stops is then recorded whole.
async fn detached<T: Send + 'static>(
    call: impl std::future::Future<Output = Result<T, String>> + Send + 'static,
) -> Result<T, String> {
    tokio::spawn(call)
        .await
        .unwrap_or_else(|error| Err(format!("the call failed: {error}")))
}

fn seconds(value: Option<i64>) -> Option<Duration> {
    value.map(|seconds| Duration::from_secs(seconds.clamp(0, 3600) as u64))
}

fn json(value: &impl Serialize) -> CallToolResult {
    match serde_json::to_string(value) {
        Ok(text) => CallToolResult::success(vec![Content::text(text)]),
        Err(error) => CallToolResult::error(vec![Content::text(error.to_string())]),
    }
}

fn outcome(result: Result<impl Serialize, String>) -> CallToolResult {
    match result {
        Ok(value) => json(&value),
        Err(error) => CallToolResult::error(vec![Content::text(error)]),
    }
}
