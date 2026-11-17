# Host tools

Nori gives every agent a set of its own tools: goals (`get_goal`, `create_goal`,
`update_goal`), git (`git_status`, `git_diff`, `git_log`, `git_commit`), the
forge (`ci_status`, `pull_request`; see [forge.md](forge.md)), and delegated
tasks (`agent_capabilities`, `delegate_task`, `task_status`, `task_wait`,
`task_send`, `task_cancel`, `task_list`). An agent built into
Nori's process gets them as **host tools**: plain function calls into Nori, with
no MCP server, port, socket or process between them. Every other agent gets the
same tools from the `nori-client` MCP server (see
[nori-client-mcp.md](nori-client-mcp.md)).

For cross-provider delegation, agents use `agent_capabilities` to discover Nori's configured
providers and models, then pass the returned values to `delegate_task`. An agent's own spawn tool
may expose only its native provider's models and is not Nori's cross-provider catalog.

Both paths serve the same `#[tool]` methods (`nori-rs/harness/src/backend/`),
so names, schemas, descriptions and replies are identical whichever way an
agent reaches them.

## How a built-in agent gets them

```text
Nori (harness)                             built-in agent (codex-acp, claude-acp)
  initialize                       ->      _meta.nori.hostTools = true
  register(tools) -> Registration
  session/new { _meta.nori.hostTools: <token> }
                                   ->      nori_host_tools::from_session_meta
                                           specs() -> the model's tool list
                                           call(name, args) when the model calls one
  tool call { _meta.nori.hostTool: "git_commit" }   <-   so the TUI knows the call
```

- The crate `nori-host-tools` (`nori-rs/host-tools`) holds the trait, the
  process-wide registry and the `_meta` keys. It depends only on std and
  `serde_json`, because Nori and its built-in agents use different ACP library
  versions.
- An agent advertises `_meta.nori.hostTools: true` in its initialize response
  only when it runs inside Nori's process (`InProcessOptions.host_tools`). The
  standalone `codex-acp` or `claude-code-acp-rs` binaries never do, because a
  token means nothing in another process.
- Nori hands host tools to a connection only when the connection is in-process
  *and* the agent advertised them. The connection then sends the token as the
  `_meta` of every `session/new`, `session/load`, `session/resume` and
  `session/fork`. The registration lives as long as the backend; dropping it
  unregisters the token.
- With host tools, Nori does not start the `nori-client` server for that
  agent, and the goal tools count as connected as soon as the session exists.

## Per agent

**Codex.** The specs go in as Codex dynamic tools (`StartThreadOptions.dynamic_tools`,
loaded up front), and the tools' instructions as the thread's own instructions
(a `ThreadInstructionsProvider`). A cold resume supplies both again. When the
model calls one, core emits `DynamicToolCallRequest`; codex-acp runs the host
tool and always answers with `Op::DynamicToolResponse`, an error included, so a
turn never waits forever. Codex's native goal tools are disabled
(`features.goals = false`), so the names cannot collide.

**Claude.** The tools are an in-process SDK MCP server named `nori`. The CLI
reaches it over the stream-json pipe it already has (`mcp_message`), and names
the tools `mcp__nori__<tool>`. They are pre-allowed in every permission mode:
they are Nori's own, and Nori enforces their policy. The git tools,
`delegate_task`, `task_status` and `task_wait` carry `anthropic/alwaysLoad`, so
they stay loaded; the rest (`agent_capabilities`, `task_send`, `task_cancel`,
`task_list`, the goal tools, the forge tools) are found through Claude Code's
tool search when needed. `pull_request` asks the user itself when the workflow
says to (see [forge.md](forge.md)), so being pre-allowed skips no question. The client's own MCP servers (stdio, HTTP,
SSE) are handed to the CLI's `--mcp-config` as they are; the CLI connects to
them itself.

## Tool results and cancellation

The in-process host boundary retains the complete MCP result as JSON, including
content blocks, `structuredContent`, and `_meta`; its text view remains
available to older callers. Claude's SDK server returns that MCP result as-is.
OpenCode and Gemini receive Nori tools through MCP and their engines handle the
MCP response. OpenCode preserves its complete tool event in ACP raw output,
forwards valid ACP content blocks, and retains unknown blocks in raw output;
replay retains the full persisted tool state. Codex dynamic tools support text, images, and audio: Nori passes media as
data URLs, represents resource and unknown blocks as JSON text, and
includes structured content and metadata as JSON text. Conversation-program
receipts retain the original MCP result along with the text view.

On Codex turn cancellation, Nori cancels the pending host-call future and
submits an error result whose structured content identifies the cancelled call
and tool. Claude's host server tracks MCP request IDs and cancels calls on an
MCP cancellation notification or a session cancellation; its cancelled result
also identifies the request and tool. Dropping a future cannot undo an effect
already performed or stop work that a tool deliberately detached; cancellation
does not promise rollback.

## In the TUI

A call carrying `_meta.nori.hostTool` is recognized by that name
(`NativeCall::from_tool`, `DelegationCall::from_tool`). A call from the MCP
path is recognized from its title, which must name `nori-client` and one of the
tools. Either way the call renders as what it did: `⎇` status and history, `±`
diff with per-file `+added −removed`, `◆` commit, `◎` goal, `◉` CI, `⇡` pull request,
and delegation as subagents.

## Adding a tool

1. Write it as a `#[tool]` method on `NoriClientService` (in
   `harness/src/backend/`), with a description that says what the call does and
   what comes back, in as few words as that takes. Policy (when to call it)
   goes in the module's `INSTRUCTIONS`, once.
2. Add its arm to `dispatch` in `harness/src/backend/host_tools.rs`. The test
   `every_listed_tool_is_dispatched` fails until you do.
3. If it deserves a view, add a `NativeCall` variant, its `from_tool` arm and a
   `view` arm in `tui/src/presentation/`, with a snapshot test.
