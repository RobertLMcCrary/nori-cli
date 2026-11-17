# ACP turn lifecycle

This document describes the current Nori harness behavior for ACP requests,
session updates, and recorded conversation history. The reducer is implemented
in `nori-rs/harness/src/backend/session_reducer.rs`; its per-session state is in
`nori-rs/harness/src/normalized/session_runtime.rs`. The canonical event journal
is described in [Transcript journal and JSONL compatibility](transcripts.md).

## Request ownership

Each ACP session has one `SessionRuntime` with one phase:

| Phase | Owner | End condition |
| --- | --- | --- |
| `Idle` | No local request | A prompt or load starts |
| `Loading` | The active `session/load` | Its response arrives |
| `Prompt` | The active `session/prompt` | Its response or transport failure arrives |
| `Prompt` with `cancelling` set | The active prompt, after Nori sent `session/cancel` | The prompt response or transport failure arrives |

The active request stores its request ID, open message buffers, tool-call IDs,
and pending permission requests. Session state such as the transcript, plan,
tool snapshots, mode, and usage survives request boundaries. The TUI receives a
flattened phase view; it does not own the ACP request lifecycle.

Nori serializes inbound ACP updates and request outcomes through the session
reducer. Updates received during a prompt or load are processed while that
request owns the session. Updates received in `Idle` remain valid unowned
activity: Nori preserves and renders them without assigning them to an earlier
or later request. Nori warns once per unowned burst, resetting that warning
when another prompt or load starts. Nori lifecycle metadata can describe an
agent-owned turn to observers, but does not claim the local ACP request slot.

ACP chunk updates do not consistently provide message identities. The reducer
therefore assembles agent, thought, and user chunks in request-local buffers and
finalizes those buffers when the owning request ends. Tool snapshots persist in
session state; request ownership is attached only when a local request created
the tool activity.

## Prompts, queues, and cancellation

A prompt submitted while the session is idle starts immediately. A prompt
submitted while another request is active is added to the in-memory FIFO queue.
Only one ACP request runs at a time. When a prompt ends with `end_turn`, the
reducer starts the next queued prompt. Other stop reasons end the active prompt
without draining the queue. A transport failure also ends the active prompt
and reports a cancelled stop reason with its failure details.

In the composer, Enter submits to this queue. Shift+Enter steers the active
turn: Nori cancels that turn and starts the steering message as soon as its
request finishes, ahead of other queued messages. If no turn is active, the
message starts immediately. Enter with an empty composer promotes the oldest
queued message by cancelling the active turn and starting that message next.
Alt+Enter inserts a newline. Queued and steering messages retain their attached
images and resources.

Submitting cancellation marks the active prompt as cancelling and sends
`session/cancel`. This does not clear the request or make the session idle. Nori
waits for the matching prompt response or transport failure before finalizing
the turn. A prompt submitted by a remote controller has a stricter path: it is
rejected as busy instead of being queued when the session is not idle.

Provider-owned child sessions maintain independent FIFO queues, including nested
children. Steering interrupts the focused child and places the steering message
ahead of its queue; promotion interrupts that child and starts its oldest queued
message next. Cancellation is sent to the focused child session. Native child
work and its queued messages prevent conversation rewind until they finish.
Each provider-owned child has a separate conversation journal. Its history and
response boundaries are scoped to that child, including typed content and tool
updates. A child rewind stages a separate agent connection and tool context from
the selected visible prefix, then keeps the same public child identity after
commit. The parent and sibling sessions keep their own histories and connections.
OpenCode child sessions are controllable as soon as they are announced: follow-up
prompts, cancellation, and configuration use the child's own session and do not
require a preceding `session/load`. Its model options are published on the child
session; loading remains a separate replay operation. OpenCode announces each
child on its immediate parent's session and sends its text, tool, and config
updates on the child's own session. A tool completing does not finish the child;
only the child's execution lifecycle changes its state.

For the OpenCode v2 engine, Nori enables agent delegation and sets its
nesting depth to 9,007,199,254,740,991 when those settings have no explicit
configuration. This is the engine's finite JavaScript integer ceiling; Nori
adds no separate nesting budget. Explicit depth and delegation rules are kept.
The adapter reads the engine's resolved configuration and, when defaults are
needed, restarts the background engine with an in-memory configuration overlay.

When a Codex or OpenCode agent asks a question, Nori presents it on the originating
session, including child sessions, and returns the selected or typed answers to
the agent. Dismissing the question cancels its pending interaction. Automatic
permission approval does not answer interactive questions.

`session/load` owns replay updates until its response arrives. A load request
received while the session is not idle is rejected with a warning. Loading
history is not reported as a completed prompt turn.

## Recording and replay

The append-only Pkl journal is the canonical conversation history. The harness
records a command before attempting its effect; a journal write failure stops
that command from running. Conversation programs record their source and each
typed operation before dispatch, then record operation results. Received ACP
and Nori session events are journaled before context observation and live
reduction. Replay reconstructs state from recorded data and does not evaluate
conversation programs or repeat external effects.

Journal record IDs remain stable and increasing within a session. A rewind
changes the visible projection but retains the earlier records and their IDs.
Fork-source records identify an inclusive source range by session ID and record
IDs, so that range remains addressable even when a later rewind hides it. A
forked conversation records its lineage in its own session journal.

Every delegated child session has its own journal and session identity. The
parent journal also records child events relayed to the parent. When Nori
replaces a provider during a handoff, the replacement uses the same recorder
and Nori conversation identity; the journal records the handoff and the new
provider binding. The companion JSONL transcript remains a compatibility and
display view and does not represent rewind projection or the latest provider
binding by itself.

## Terminal program status

The interactive Nori ACP client reports its active conversation through the
[Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status)
(OSC 7501). It reports the root turn and each announced child session as separate
records, retaining the parent-child hierarchy. Terminals that do not implement the
protocol ignore the reports. Permission and sign-in requests are reported as
blocked; cancellation returns to idle. Permission requests use `kind=permission`,
and sign-in flows use `kind=auth`. Interactive questions transported by
`_nori/request_user_input` use `kind=question` on the originating root or child
record. Answering or withdrawing the exact request clears its blocked state;
tool names and text alone never imply a question.
Nori clears a child record and its descendants when the agent reports that the
child session closed; cancellation of a turn instead leaves the child idle. It
clears all records when the root session closes or the client exits. Reports do
not contain percentage estimates.
