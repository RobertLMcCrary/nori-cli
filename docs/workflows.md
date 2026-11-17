# Workflows

A workflow configures follow-up prompts, commit and pull request guidance, and
fallback models for an ACP session. Workflow policy files are Pkl modules; they
return data for the harness to apply. Evaluating policy does not dispatch agent
messages or repeat conversation effects. Terms follow [glossary.md](glossary.md).

## Policy sources

| Source | Path | When it loads |
| --- | --- | --- |
| Built-in defaults | `nori-rs/pkl/default.pkl` | First |
| User policy | `$NORI_HOME/init.pkl` | Every session, when present |
| Project policy | `<repo>/.nori/nori.pkl` | After user policy, only in a trusted directory |

Policies apply in that order. Later modules override fields they set. The built-in
policy is compiled into Nori, so a session with no custom policy does not start
Pkl; a test compares that value with `default.pkl`. A missing file is skipped. User policy modules have normal Pkl file and environment
resource access, so they can use Pkl imports and reads directly.

The `nori-script` Steel interface remains only for compatibility with legacy
code; ACP policy loading uses Pkl and shared `nori-workflow` data types.

A policy module returns top-level values. Unset values keep those from the
previous source. For example, a project policy can replace `fallback_models`
without replacing the global policy's hooks. The nested `commit`, `pr`, `hooks`,
`instructions`, and `fallback_models` values are replaced as whole values when
set. Unknown policy fields are errors.

## Policy fields

| Field | Type | Meaning |
| --- | --- | --- |
| `name` | `String` | Workflow name. |
| `max_turns` | `Int` | Maximum hidden hook turns after a user turn. Omit it for no policy cap; `0` disables hook turns. Negative values are invalid. |
| `instructions` | `List<String>` | Standing instructions included with workflow prompts. |
| `hooks` | `List<Dynamic>` | Ordered `{ event, prompt }` rules. Events: `on-change`, `on-turn`, `on-commit`, `on-pr`, `on-stop`, `on-ci-fail`, `on-ci-pass`. |
| `fallback_models` | `List<String>` | Candidate model IDs, in order, for subscription quota exhaustion. |
| `fallback_ranked` | `Boolean` | Choose fallback candidates from the advertised model catalog. |
| `commit` | `Dynamic` | Commit style and batching options. |
| `commit_reviewer` | `Dynamic` or `null` | Agent and optional model for background commit review. |
| `pr` | `Dynamic` | Pull request content and upstream policy. |
| `tools` | `Dynamic` | Session selection, descriptions, and instructions for Nori-owned host tools. |

The built-in policy selects ranked fallback models and leaves `max_turns`
unset. There is no default hook-turn budget. A `max_turns` value is an explicit
policy cap; it does not cap a normal model turn or its final answer.

When an ACP agent reports structured usage or billing quota exhaustion, Nori
first tries known equivalent routes for the current versioned model, including
different provider routes exposed by one ACP agent, then tries the configured
models in order while skipping providers already exhausted in the conversation.
Ranked candidates come from the cached advertised models and
use the same ordering as `/model`. The selected provider opens a fresh ACP
session with a checkpoint of the visible conversation and retries the prompt.
The conversation keeps its canonical journal and identity; private provider
session state cannot be transferred. Temporary rate limits and unrelated errors
do not trigger fallback. If no candidate succeeds, Nori shows the original
error. For OpenCode, Nori reads explicit provider codes from the structured
HTTP error body; a generic HTTP 429 or a transient rate-limit code does not
mark the provider exhausted.

### Nori host tools

The optional `tools` object selects Nori-owned host tools before the ACP agent
receives its catalog. It does not alter the agent's built-in tools or other MCP
servers. `enabled`, when present, is an allowlist (an empty list selects none);
`disabled` removes names from that list or from the default catalog.
`descriptions` overrides model-facing descriptions by name, and `instructions`
appends model-facing guidance to the generated list of selected tools. Later
policy sources replace each supplied field independently.

```pkl
tools = new Dynamic {
  enabled = List("get_goal", "git_status")
  disabled = List("git_status")
  descriptions = new Mapping { ["get_goal"] = "Read the current goal" }
  instructions = List("Check the goal before reporting completion.")
}
```

Only selected tools are advertised and callable. If `get_goal` or `update_goal`
is disabled, Nori disables its `/goal` command and goal continuations unless
the agent provides its own goal extension. A customized catalog cannot
reuse a Claude process prewarmed with a different catalog. Nori loads policy
once before creating the agent session when it can receive Nori tools, and uses that same policy and captured
machine snapshot for its prompts and tools. Agents without Nori tools can start
while the policy loads; prompt admission waits for it.

### Commit options

`commit` may set `style` (`plain` or `conventional`), `max_files`,
`max_subject`, `atomic`, `batch` (`per-change`, `per-turn`, or `per-task`),
`require_body`, and `agent` (`{ agent, model? }`). The `agent` field delegates
commit writing to that configured ACP agent.

### Pull request options

`pr` may set `title`, `sections`, `max_lines`, `max_files`, `base`, `draft`, and
`upstream`. The upstream value is `never`, `ask`, `tests-pass`, or `done`.

## Machine properties

Pkl modules can read the machine snapshot passed by Nori through external
properties:

| Property | Value |
| --- | --- |
| `machine.cpu.available_parallelism` | Logical CPUs available to the process. |
| `machine.cpu.load_average.one_minute` | One-minute system load average. |
| `machine.cpu.load_average.five_minutes` | Five-minute system load average. |
| `machine.cpu.load_average.fifteen_minutes` | Fifteen-minute system load average. |
| `machine.memory.total_bytes` | Total visible memory, in bytes. |
| `machine.memory.available_bytes` | Available memory, in bytes. |

Read a property with Pkl's `read("prop:<name>")`; values are strings, so parse
numeric readings with `toInt()` or `toFloat()`. Unsupported readings are the
literal string `unsupported`, rather than zero. For example:

```pkl
local available = read("prop:machine.memory.available_bytes")
local enoughMemory = available != "unsupported" && available.toInt() > 8_000_000_000

name = "memory-aware"
instructions = if (enoughMemory) List("Run the full test suite") else List("Run focused tests")
```

Memory values are the system values reported by sysinfo; they may not reflect a
container's memory limit. CPU availability uses Rust's process-available
parallelism estimate.

Nori captures one snapshot before evaluating policy, journals its serialized
value, and passes the same values to both workflow and fallback policy loads.
When resuming a recorded session, it reuses that snapshot instead of reading
machine state again. Replaying the conversation reduces recorded data and never
evaluates policy modules or repeats effects.

## Example

`$NORI_HOME/init.pkl`:

```pkl
name = "standard"
fallback_ranked = false
fallback_models = List("provider/preferred-model", "provider/backup-model")
instructions = List("Run the tests that cover each change")
hooks = new Listing {
  new Dynamic { event = "on-stop"; prompt = "Summarize the work and remaining risks" }
  new Dynamic { event = "on-change"; prompt = "Review the diff and commit focused changes" }
}
commit = new Dynamic {
  style = "conventional"
  batch = "per-change"
  atomic = true
}
pr = new Dynamic {
  sections = List("Why", "What", "Testing")
  base = "main"
  upstream = "ask"
}
```

Replace illustrative model IDs with models advertised by the candidate agent.
A structured subscription quota error triggers fallback in order; unrelated
errors and short-term rate limits do not. A fallback starts a new provider
session with the failed prompt and a checkpoint of visible conversation
context. It cannot inherit provider-private session state.

A commit reviewer can be selected with:

```pkl
commit_reviewer = new Dynamic { agent = "claude"; model = "haiku" }
```

The reviewer runs as a background subagent after a changed user turn. It is
asked to review staged, unstaged, and untracked changes and suggest commit
messages without modifying files or committing. The working tree may contain
older changes; this instruction is not a read-only sandbox.
