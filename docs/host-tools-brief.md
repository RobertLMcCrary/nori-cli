# Brief: host-provided tools, and everything else still open

Status: written 2026-11-15 on branch `perf/instant-core` (HEAD `3b8cb932`). Everything under "Verified" was read in the code or run today. Everything under "Assumed" is a claim nobody has checked, and the first task is to check it.

---

## 1. Why this exists

Nori gives every agent a few tools of its own: git status and commit, goals, and delegated tasks. Today they reach the agent as an **MCP server**.

- Nori starts an HTTP (or stdio) MCP server, `nori-client`, in `harness/src/backend/nori_client_mcp.rs`.
- `register_for_session(.., cwd)` (line ~639) appends it to the `mcp_servers` list of each `session/new`.
- The agent connects back to it like any third-party MCP server.

This worked, but it has three costs, and the user has asked to remove them:

1. **Latency and moving parts.** Each new session pays for an MCP handshake and a connection back into Nori, per agent. The user's goal is that starting an agent, switching agent, model or subagent is "instant" (initialize under 200ms).
2. **Ugly in the UI.** The agent names the tool its own way (`mcp__nori-client__git_commit`, `nori-client.git_commit`, ...), and `tui/src/presentation/native_tool.rs` recognises each spelling by substring. A new spelling means a new bug.
3. **Philosophically wrong.** The user's words: *"I believe tools like t3 don't even do things through an mcp server at all."* Nori and its agents now share one process. A localhost HTTP hop between two things in the same address space is pure overhead.

**Goal:** Nori hands the agent a tool list and executes the calls itself, in-process, with no MCP server. External agents that cannot take a host tool list keep the MCP server as a fallback.

### Assumed, to verify first (Phase 0)

- **What t3 actually does.** I never read t3's source. The user believes it does not use MCP. Before designing anything, find t3's tool path (likely in its agent/provider layer), and write down in this file: how a tool is declared, how a call comes back, how it is rendered. If t3 turns out to use MCP, this brief's motivation shrinks to costs 1 and 2, which still stand. The design below does not depend on t3 being right.
- **That MCP is the measurable latency cost.** Nobody timed `session/new` with and without the `nori-client` entry. Measure first (see Phase 0 below), so the win is a number rather than a belief.

---

## 2. What exists today (verified)

### 2.1 The tools themselves

| Tool | Where | Notes |
|---|---|---|
| `get_goal`, `create_goal`, `update_goal` | `harness/src/backend/nori_client_mcp.rs` (~178-225) | Thread goal state; `respond_with_goal`, `goal_json` helpers |
| `git_status`, `git_commit` | `harness/src/backend/git_tools.rs` (111 lines) | Backed by gix (`nori-config/src/git_repo.rs`, `git_write.rs`); `git_commit(paths, message)` commits exactly those paths |
| delegated-task tools incl. `delegate_task` | `nori_client_mcp.rs` (~246, ~998) | Per-session task registry |
| `register_custom_acp_agent` | prompt/tool list seen in logs | Registers a custom ACP agent |

All are `rmcp` `#[tool(...)]` methods on a `NoriClientShared` that carries per-session state (`with_cwd(cwd)`).

### 2.2 How the TUI renders them

- `tui/src/presentation/native_tool.rs` (560 lines): `NativeCall` (GitStatus, GitCommit{paths,message}, GetGoal, CreateGoal, UpdateGoal), `NativeCall::recognize(title, raw_input)`, `view(call, active, failed, result) -> NativeView{glyph, header, detail}`, tones, glyphs (`⎇` `◆` `◎`).
- `tui/src/presentation/mod.rs` ~681: `Invocation::Delegation` first, then `Invocation::Native`, then the generic path.
- `tui/src/.../client_tool_cell.rs::render_native_lines`: tree connectors, spinner while active, red glyph on failure, green `◆` on success, cyan otherwise.
- The **recognition** step is the fragile part: it starts from the agent-chosen tool title. A host tool should arrive already identified, so recognition becomes a lookup by a stable id, not substring matching.

### 2.3 The two in-process agents, and their extension points

**Codex (vendored in `nori-rs/codex-acp`, Codex rust-v0.169.0).**
- Codex core already has the mechanism we want: **dynamic tools**.
  - `codex_protocol::dynamic_tools::DynamicToolSpec` = `Function{name, description, input_schema, defer_loading}` or `Namespace{name, description, tools}`.
  - Specs go in at thread start (`thread_manager.rs:371` has a `dynamic_tools` field in the thread-start params) or later via `CodexThread::replace_dynamic_tools` (`Op::ReplaceDynamicTools`).
  - When the model calls one, core emits `EventMsg::DynamicToolCallRequest{call_id, turn_id, namespace, tool, arguments, started_at_ms}`.
  - The host answers with `Op::DynamicToolResponse{ id, response: DynamicToolResponse{ content_items: [InputText|InputImage|InputAudio], success } }`.
- `codex-acp/src/thread.rs` already **receives and renders** those events (`EventMsg::DynamicToolCallRequest` at ~1298, `end_dynamic_tool_call` at ~1782). What it does *not* do is answer them; today the answer path is whatever upstream Zed shipped.
- Today Nori's tools reach Codex as ordinary MCP via `codex_agent.rs` ~368-385 (`mcp_servers` merged into the config).

**Claude (vendored in `nori-rs/claude-acp`, driving the `claude` CLI over stream-json).**
- The driver already contains an **in-process SDK MCP server**: `claude-acp/src/mcp/acp_server.rs` implements `SdkMcpServer` with `SdkMcpTool`/`ToolDefinition`/`ToolHandler`. It is how the driver's own tools (Edit, glob, ...) are exposed. The CLI sees them as an "SDK" server and sends `mcp_message` control requests back over the stream-json pipe. No sockets, no HTTP.
- So Claude already has a no-network, in-process tool path. Host tools can be registered the same way as additional `SdkMcpTool`s.
- Caveat: the Claude CLI will still *name* them `mcp__<server>__<tool>`. That naming is the CLI's, so the TUI must recognise by server name there (cheap, and fully controlled).

### 2.4 Shared plumbing already in place

- In-process agents run over `tokio::io::duplex` (`acp-host/src/in_process.rs`, `acp_connection.rs::spawn_in_process`); the agent end gets a `DuplexStream`, Nori keeps the other end.
- Per-agent launch options flow through `InProcessSpec{agent, home, config}` and `codex_overrides_from_env` / `claude_overrides_from_env`.
- `nori-rs/cli/src/native_agents.rs` registers both agents at startup.

---

## 3. Target design

### 3.1 One trait, owned by the host

```text
HostTools   (in a small crate both agent crates can depend on, e.g. nori-host-tools)
  fn specs(&self, session: &SessionCtx) -> Vec<HostToolSpec>      // name, description, JSON schema
  async fn call(&self, session: &SessionCtx, name: &str, args: Value) -> HostToolResult
HostToolResult { text: String, structured: Option<Value>, success: bool }
SessionCtx   { session_id, cwd, goal handle, task registry handle, event sender }
```

- The existing `NoriClientShared` logic (goal, git, tasks) moves behind `HostTools` unchanged. The `#[tool]` rmcp wrappers become one thin adapter over it, kept for external agents.
- **Constraint to respect:** the two agent crates use different ACP library versions (Nori on `agent-client-protocol` 0.15.1, the embedded crates on 0.14/sacp). Coupling is bytes-only today. `HostTools` must therefore use only std + `serde_json` types, never ACP or rmcp types, so it can be shared without dragging versions together.

### 3.2 Codex: dynamic tools

1. Build `Vec<DynamicToolSpec>` from `HostTools::specs`; pass in the thread-start params for new/resumed threads, and on model/session switch call `replace_dynamic_tools`.
2. In `codex-acp/src/thread.rs`, on `DynamicToolCallRequest`: call `HostTools::call`, send `Op::DynamicToolResponse{id: call_id, response}`. Keep the existing ACP tool-call start/update notifications so history and the TUI still see the call.
3. Remove the `nori-client` MCP injection for Codex in `codex_agent.rs`.
4. Tool identity: set `meta` on the ACP tool call (e.g. `{"nori":{"hostTool":"git_commit"}}`) so the TUI recognises by that field, not by title.

### 3.3 Claude: SDK tools

1. Register each `HostToolSpec` as an `SdkMcpTool` on the existing in-process server in `claude-acp/src/mcp/acp_server.rs`, with the handler calling `HostTools::call`.
2. Permission: the driver's `can_use_tool` runs for MCP tools; host tools should be pre-allowed (they are the host's own).
3. Stop injecting the `nori-client` HTTP entry for the native Claude driver.
4. Tool identity: the CLI names them `mcp__<server>__<tool>`; use a reserved server name (e.g. `nori`) and map it in the TUI, plus the same `meta` field if the driver can attach it.

### 3.4 External agents (gemini, opencode, custom, remote)

Keep the MCP server exactly as it is, behind the same `HostTools`. `register_for_session` only adds it when the agent is **not** native. No behaviour change for them.

### 3.5 TUI

- `NativeCall::recognize` keeps substring matching only as the fallback for the external-MCP path; the primary path is the `meta.nori.hostTool` id.
- Adding a tool becomes: implement it in `HostTools`, add one `NativeCall` variant + `view` arm. Nothing about spellings.
- Keep the visual language (glyphs, tree connectors, tones) and the style guide: avoid yellow/blue; cyan = info, green = success, red = error (`tui/styles.md`).

### 3.6 Tools worth adding once the path exists

These are what make the design earn its keep and are why the user asked for it:

- `git_diff(paths?)` / `git_log(n)`: read-only, from `GitRepo` (the byte-identical `diff_text` already exists).
- `git_branch_create`, `git_worktree_create`: from `utils/git/src/worktree.rs`.
- `ci_status`: reads PR/CI state (see §4.4).
- `commit_plan(prompt)` / `batch_commits`: the "designated agent that writes and batches atomic commits" feature; drives repeated `git_commit` calls.
- `ask_user` / `notify` (a toast, see §4.7): needs the event sender in `SessionCtx`.

---

## 4. Work breakdown

### Phase 0: verify (small)
- [ ] Read t3's tool path; record findings in this file (§1 "Assumed").
- [ ] Time `session/new` with and without the `nori-client` entry for Codex and Claude (use `scratchpad/acp_probe.py` or the existing `init_time.py`); record the delta. Target: initialize < 200ms, new-session overhead of host tools ≈ 0.
- [ ] Confirm that Codex honours dynamic tools through the embedding API in `codex-core-api` at 0.169 (the field exists in thread start; confirm `codex-acp::load_agent` can set it).

### Phase 1: extract
- [ ] New crate with `HostTools`, `HostToolSpec`, `HostToolResult`, `SessionCtx` (std + serde_json only).
- [ ] Move goal/git/task logic out of the rmcp wrappers; rmcp adapter calls the trait. All existing tests in `nori_client_mcp.rs` keep passing untouched (they are the regression net).

### Phase 2: Codex
- [ ] Pass specs at thread start and on switch; answer `DynamicToolCallRequest`; drop MCP injection for native Codex.
- [ ] Test: unit test with a stub `HostTools`; live test in Rex that `git_status` is called and the cell renders (use an isolated `NORI_HOME`; the account is rate-limited until Dec 8, so a live Codex test may be impossible. Plan for a mock model or an offline test of the event round trip).

### Phase 3: Claude
- [ ] Register `SdkMcpTool`s, pre-allow, drop the HTTP injection for native Claude.
- [ ] Live test (this works today): ask "what files changed?" in a dirty repo and confirm `git_status` renders as a first-class cell.

### Phase 4: TUI + cleanup
- [ ] Recognise by `meta.nori.hostTool`; keep substring fallback for external MCP.
- [ ] Snapshot tests for each native cell in the new path.
- [ ] Update `docs/` with the tool list and how to add one.

### Done when
- Native Codex and native Claude sessions contain **no** `nori-client` MCP entry and the tools still work and render identically.
- External agents behave exactly as before.
- Measured `session/new` overhead of host tools is within noise of zero.

### Risks
- Dynamic-tool behaviour (deferred loading, namespacing, whether a spec change mid-thread invalidates the prompt cache). Test `replace_dynamic_tools` cost before using it on every model switch.
- Claude CLI naming and the permission path for SDK tools: confirm `can_use_tool` really gets bypassed or auto-allowed.
- Version skew: keep the trait free of ACP/rmcp types (see §3.1).
- Resumed sessions: tool specs must be re-supplied on resume; make sure `ThreadStore`-resumed threads get them.

---

## 5. Everything else still open

### 5.1 Verification debts (things I did *not* see working)
- **Toasts have never been seen on screen.** Unit tests cover the queue, expiry and drawing; session-info toasts are covered at the event level. In a live Rex run, native Claude sent no session title, and the commit keybind does not produce a toast, so nothing triggered one. Needs a real trigger (a fixture/mock agent that sends a `SessionInfoUpdate` title, or a Claude run that produces one) and a screenshot check of the top-right position, wrap, and expiry.
- **Codex 0.169 never completed a turn.** It connects, authenticates, lists GPT-5.6 models and sends the request; the account's ChatGPT usage limit (until Dec 8, 2026) then ends it. So tool calls, approvals (the new `ReviewDecision` variants, `Denied{rejection}`), image generation and the `Op::TurnInput` steering path are compiled but untested end to end. Needs a different account, a mock model provider, or Dec 8.
- **Ctrl+Shift still unconfirmed.** I added a tolerant matcher (`nori/hotkey_match.rs::modifiers_match`: uppercase char with SHIFT folded in), and the Rex terminal does send `CSI 110;6u` for ctrl+shift+n. I could not reproduce the user's failure. Ask which terminal and which key; if it persists, log the raw `KeyEvent` and check the kitty flags (DISAMBIGUATE | REPORT_EVENT_TYPES | REPORT_ALTERNATE_KEYS).
- **"Doesn't render to the bottom" on launch.** Not reproduced. Ask for a full-terminal screenshot and terminal size if it persists.
- **Nix.** `.#checks.aarch64-darwin.module` builds (now with a stub package in the check). `.#nori` was still fetching git inputs (openai/codex is a full clone) when this was written; result unknown. Also never run: `nix flake check` as a whole, the adapters check, Linux builds. The package disables tests (`doCheck = false`) and builds only `--bin nori`.

### 5.2 Code debts
- **codex-acp translation tests are disabled** (`#[cfg(any())]` on `mod tests` and `with_client` in `codex-acp/src/thread.rs`). Since 0.169 `Op` is neither `Clone` nor `PartialEq`. Rebuild the stub thread around `Op::TurnInput` (reply channel) and re-enable. These covered event translation, so this is real coverage loss.
- **Event-translation of new 0.169 events.** Fields I ignored or defaulted while porting: `ExecCommandEndEvent` no longer carries stdout/stderr/formatted_output (so command output shown in the ACP tool update may now be only `aggregated_output`; check it renders), `ImageGenerationEndEvent` no longer has `result` (image payload is `image: GeneratedImageRef`; I currently pass an empty string, so generated images likely don't display; fix), `guardian_action_summary` has a catch-all `_ => None`, and `ReviewDecision::ApprovedMcpPolicyAmendment` got a placeholder label.
- **`PERF_PLAN.md`** is untracked at the repo root; decide to commit or delete.
- **Leftover shell-outs.** Production git is gix-only. `git` still appears in tests/e2e fixtures as an oracle (intended). `nori-skillsets` and `nori-ai` external commands are still shelled out.
- **Linker warning** `__eh_frame section too large` on the dev `nori` binary; harmless but noisy. Likely fixed by a lighter dev profile or `-C link-arg=-Wl,-ld_classic`; low priority.
- **`remote_control::tests::disable_disconnects_the_controller_and_stops_every_listener`** and `nori-acp-host remote_ws one_server_listens_...` fail in this environment (ports/sandbox); shell-detection tests fail because of the nix zsh path. Triage as environmental or fix.

### 5.3 Claude driver: "amazing" backlog
**Decision (2026-11-15): the driver keeps spawning the unmodified `claude` CLI.** An in-process engine was considered and dropped. Subscription logins may only be used through the unmodified Claude Code binary (code.claude.com/docs/en/legal-and-compliance forbids third-party tools routing requests through subscription credentials or storing tokens), and an API-key-only engine would leave subscription accounts on the CLI anyway. Revisit only if the user wants a Console-key path.

Done since this list was first written: mode and model are advertised as config options and changed via `session/set_config_option`; a model chosen between prompts is applied after the next query.

Latency and robustness already done (warm `prepare_session`, `skip_version_check`, descendants reaper, EOF exit). Still to do:
- Make session config options (not just mode/model) first class: `SetSessionConfigOptionRequest` is in the ACP schema and the Nori TUI can drive it, but the driver only routes `session/setMode` and the unstable `session/set_model`. Add `session/set_config_option` and advertise `config_options` in `NewSessionResponse`.
- Tolerant frame parsing: the log shows `Skipping a message type the SDK does not recognise` for `rate_limit_event`. Surface it (a toast; it carries reset info) instead of dropping it.
- Control-request error replies and permission-mode mapping (ideas from zeron).
- Account selection: `cli_path` reaches the driver via `config = ["cli_path=..."]` in the `native` distribution; make Nori fill it from the active account automatically.
- In-process there is no reaper (it belongs to the standalone binary); confirm a Nori crash cannot leave `claude` children behind (process-group kill on drop).
- Warm-start numbers: re-measure initialize and first-token time natively vs. the external adapter and put them in `docs/`.

### 5.4 The git/PR/CI feature (from the user's original ask)
Partially built: gix-backed status/diff/commit, `[git]` config, automatic commit prompt, alt+g keybind (`CommitChanges`). Missing:
- **A designated commit agent.** Config to name which agent (and model) writes and batches atomic commits from a prompt, rather than the current session agent.
- **PR rules.** A config/AGENTS-style file stating what a PR should look like (title format, body sections, size limits) and when changes should be upstreamed; enforce/offer at PR time. No `/commit` or `/pr` slash commands (explicit user rule): automatic plus a keybind only.
- **CI.** Read check status for the branch's PR and surface it (tool `ci_status`, footer glyph). Needs a forge client in Rust; no shelling to `gh`.
- **Worktrees** are gix-based already (`utils/git/src/worktree.rs`); wire "run this task in a fresh worktree" through the delegated-task path.

### 5.5 Agent switching speed (user goal: instant, < 200ms initialize)
- Measured earlier for Claude; not re-measured after vendoring and the Codex 0.169 move. Re-measure all agents: initialize, session/new, first token, and switching agent/model/subagent, including across different subscriptions/accounts.
- Pre-warm: keep a second agent connection hot for the most likely switch target.
- OpenCode integration is requested and only partly done (adapter in the Nix module). Needs the same native/fast path or at least measured parity.

### 5.6 Pure-Rust stack
- `fancy-regex` and a TLS provider better than `ring` (aws-lc-rs or rustls-rustcrypto) were requested. Confirm what the dependency graph actually uses now (`cargo tree -i ring`) and finish the swap if not done; Codex 0.169 pulls its own choices, so check for duplicates.

### 5.7 Toasts and styling
- Toast overlay lives in `tui/src/tui/toast.rs` and `tui.rs` (`show_toast`, `draw_frame`); config key `[tui] toasts` (default true), `NoriConfig.toasts`.
- Candidate toast sources beyond session titles: rate-limit warnings (from the Claude `rate_limit_event`), agent switched, commit landed, CI result. Keep to the style guide.
- "More fun with formatting": native tool cells are done; other cell types (diffs, exec output, errors) still use the older look.

### 5.8 Housekeeping
- Memory/disk: only one release/fat-LTO build at a time; nix with `--max-jobs 1 --cores 4`; close browser processes first. Disk was at 8-14 GB free during this work; `nori-rs/target` is regenerable and the first thing to clear.
- Restore the user's real Rex tab to the final binary after the last build.
- Repo state: branch `perf/instant-core`, three commits on top of `200c08c7` (`04a9421c` toasts+clear+error detail, `4dc5d4dd` Claude native driver, `3b8cb932` Codex 0.169). `Cargo.lock` was regenerated; `/tmp/nori-Cargo.lock.pre169` may still hold the previous copy.

---

## 6. Suggested order

1. Phase 0 (t3 read + timing). Cheap, and it can change the design.
2. Rebuild the codex-acp test stub (§5.2), so Phase 2 has a safety net.
3. Phases 1, 3, 2 (Claude before Codex, because Claude can be tested live today and Codex cannot).
4. Phase 4 and the new tools (§3.6), then the commit-agent / PR rules / CI work (§5.4), which depends on them.
5. Verification debts (§5.1) as accounts and terminals allow; Nix result whenever the build lands.
