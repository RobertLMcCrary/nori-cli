# Conversation programs

The ACP harness exposes a `conversation` host tool. Its `source` argument is a
Pkl module returning ordered command data. `HarnessHandle` exposes the same
runtime through `execute_conversation_program` and
`execute_conversation_program_path`. File programs retain their original URI
for relative imports. The recorded main-module source is the source evaluated.

```pkl
commands = new Listing {
  new Dynamic { op = "spawn"; actor = "reviewer"; agent = "claude" }
  new Dynamic { op = "send"; recipient = "reviewer"; message = "Review the implementation and report defects" }
}
```

`send` queues child work and does not wait or return a task ID; there is no
`wait` program operation. `prompt` accepts only `text` and targets the current
session. For a separate tracked delegation with a result, use `delegate_task`
and `task_wait` or its completion message. A task ID from a spawn is not a
receipt for a later actor send.

Actor names resolve to registered child session IDs. The runtime records these
bindings so later programs can address the same child. A program's own `spawn`
bindings take precedence inside that program, even if a concurrent program uses
the same name. A later program without a local binding uses the most recent
visible recorded binding. A successful result
contains a unique `program_id`, the number of applied commands, and any command
results. Evaluation produces data; the harness performs effects afterward.

For delegation with an explicit task, model, effort, role, and isolation, call
the typed `delegate_task` host tool through `tool-request`. Its input uses the
same schema the model receives from the host-tool catalog:

```pkl
import "pkl:json"
commands = new Listing {
  new Dynamic {
    op = "tool-request"
    id = "review-change"
    tool = "delegate_task"
    input = json.render(new Dynamic {
      task = "Review the changed files and identify concrete defects"
      agent = "codex"
      model = "gpt-6-luna"
      isolation = "shared"
      mode = "background"
    })
  }
}
```

Choose model IDs and configuration options from advertised capabilities.

| Operation | Fields | Effect |
| --- | --- | --- |
| `prompt` | `text` | Queue a main-session prompt. |
| `spawn` | `actor`, `agent` | Start a background child and return its session ID. |
| `send` | `recipient`, `message` | Send a follow-up to a child ID or actor name. |
| `config` | `key`, `value` | Apply an advertised ACP configuration option. |
| `select-context` | `start`, `end` | Return the inclusive canonical event range. |
| `fork` | `actor`, `start`, `end` | Start a child from a source conversation range; `self` means the current journal. |
| `rewind` | `head` | Move the visible head and bind a fresh provider session. |
| `tool-request` | `id`, `tool`, `input` | Call a registered native host tool; `input` is a JSON string. |
| `tool-result` | `id`, `output` | Record externally supplied result data. |

An explicit external process is expressed as a top-level declaration:

```pkl
unsafe {
  program = "printf"
  args = List("diagnostic\n")
}
```

It runs after the ordered commands. Optional `cwd` selects its working
directory. Arguments go directly to the executable. Process output includes
text, exit status, and base64 bytes. A failed process or host tool stops the
program after recording its error; previously completed effects remain.

The runtime appends the program and each operation before attempting its
effect. It records outcomes separately, including a program-level success or
failure receipt for an empty program or failed evaluation. Each send records its
resolved child session before delivery. Reopening a journal only reduces
recorded data: it never evaluates old programs, repeats tool calls, or starts
processes. Append-before-effect makes intent auditable, but does not make an
external process exactly-once across a crash between its effect and result.
A rewind is rejected while a program effect is running. A program that began
before another rewind cannot dispatch further operations into the new session;
a program may continue after its own successful rewind.

The canonical `.pkl` journal retains rewound records. Transcript summaries and
replay derive from the current projection; explicit source ranges can still
refer to retained history. The JSONL transcript is a compatibility projection.
`ConversationStore` separates append/replay from storage, with versioned writes
and command deduplication; the current implementations are file and memory
stores. A remote daemon transport is not implemented yet.

Programs receive the same captured [machine properties](workflows.md#machine-properties)
as workflow policy. Replayed data uses recorded observations. Normal Pkl file
and environment reads remain available during fresh evaluation, so external
imports and resources are not a self-contained reproducible snapshot.
