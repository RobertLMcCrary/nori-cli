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
pub(crate) const INSTRUCTIONS: &str = nori_host_tools::DELEGATION_INSTRUCTIONS;

/// `agent_capabilities` re-reads agents whose entry is older than this...
const CAPABILITIES_FRESH_FOR: Duration = Duration::from_secs(120);
/// ...waiting at most this long for an agent Nori has never read...
const CAPABILITIES_WAIT: Duration = Duration::from_secs(20);
/// ...and this long when every agent was read before, answering with that if the agents are
/// slow; the refresh goes on in the background.
const CAPABILITIES_STALE_WAIT: Duration = Duration::from_secs(3);

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
        description = "List the agents you can delegate tasks to, with the models and other settings (such as reasoning effort) each offers, plus your current delegation depth and running-task count.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn agent_capabilities(&self) -> Result<CallToolResult, McpError> {
        let inner = self.orchestrator().inner();
        let never_read = crate::registry::list_available_agents()
            .into_iter()
            .filter(|agent| agent.is_installed)
            .any(|agent| {
                super::catalog::remembered(&inner.seed.config.nori_home, &agent.agent_name)
                    .refreshed_at
                    == 0
            });
        // Re-read agents not read in the last two minutes, for as long as an answer can wait.
        let _ = tokio::time::timeout(
            if never_read {
                CAPABILITIES_WAIT
            } else {
                CAPABILITIES_STALE_WAIT
            },
            super::probe::refresh(
                Arc::clone(&inner.seed.config),
                inner.seed.cli_version.clone(),
                CAPABILITIES_FRESH_FOR,
            ),
        )
        .await;
        Ok(json(&inner.capabilities()))
    }

    #[tool(
        description = "Start a task on a child agent (choices: `agent_capabilities`). The child sees only `task`, not this conversation, and cannot ask questions. Returns a `task_id` at once; the result arrives as a message, so don't poll, and use `mode: wait` only if you cannot go on without it. `isolation: shared` edits your files directly: give parallel children disjoint files, or use `isolation: worktree`. No secrets in `task`.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true),
        meta = always_load()
    )]
    pub(crate) async fn delegate_task(
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
            fork_context: None,
            fork_source: None,
            fork_entries: Vec::new(),
            parent_view: None,
        };
        let inner = Arc::clone(self.orchestrator().inner());
        Ok(outcome(
            detached(async move { inner.delegate(delegation).await }).await,
        ))
    }

    #[tool(
        description = "Read one delegated task: `status`, the child's current `activity` and `tool_calls` while it runs, the files its edit tools changed, and its final message once finished. Reading a finished task delivers its result, so it is not also sent as a message.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false),
        meta = always_load()
    )]
    pub(crate) async fn task_status(
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
        description = "Block until the listed tasks finish, one awaits the user's approval, or the timeout passes. Only when you cannot continue without the results; they otherwise arrive as messages.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false),
        meta = always_load()
    )]
    pub(crate) async fn task_wait(
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
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn task_send(
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
                        super::launch::FollowUp::Tool,
                        None,
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
        )
    )]
    pub(crate) async fn task_cancel(
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
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn task_list(&self) -> Result<CallToolResult, McpError> {
        Ok(json(&TaskList {
            tasks: self.orchestrator().inner().list().await,
        }))
    }
}

#[derive(Serialize)]
struct TaskList {
    tasks: Vec<super::task::TaskSnapshot>,
}

/// Asks Claude Code to keep a tool loaded rather than behind its tool search, for the tools an
/// agent should always know it has. The rest are found by search when needed.
pub(crate) fn always_load() -> rmcp::model::Meta {
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
