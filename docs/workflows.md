# Workflows

A **workflow** says how an **agent** behaves in a **session**: how many
**prompt turns** it may run on its own, what Nori prompts it with at session
events, and the rules its commits and pull requests follow. Workflows are
written in a small Scheme vocabulary, `nori/workflow`, evaluated by an embedded
and sandboxed [Steel](https://github.com/mattwparas/steel) engine (crate
`nori-script`). The result is plain data; nothing outside `nori-script` sees
Scheme. Terms follow [glossary.md](glossary.md).

## Where workflows come from

| Source | Path | Loaded |
| --- | --- | --- |
| Global script | `$NORI_HOME/init.scm` | Always, when present |
| Project script | `<repo>/.nori/nori.scm` | Only when the directory is trusted |
| Inline DSL | The user's prompt | Per prompt, against the loaded scripts |

The layout follows Helix's Steel plugin system (`~/.config/helix/init.scm` plus
`helix.scm`, with builtin `helix/...` modules). Nori's builtin module is
`nori/workflow`, and it is already loaded: scripts do not `require` it.

The global script runs first and the project script second, in one engine. The
project script sees every global definition and overrides it:

- Registering a workflow under an existing name replaces it.
- The last `default-workflow` wins.
- `(define-workflow land (use 'land) (turns 1))` derives from the global
  `land`. Quote the name: while `land` is being redefined, the bare variable
  `land` does not hold the old workflow yet.

A missing file is skipped. With no script and no inline DSL, no engine is
created.

Creating a Steel engine dominates the cost of loading: about 100 ms in an
optimized build (2 to 3 s in a debug build), against about 1 ms to run a
typical script. `nori_script::ScriptHost` therefore loads on its own thread as
soon as it is spawned, off the session's start-up path, and keeps that engine
for inline DSL, which then costs under 1 ms per prompt.
`Loaded::apply_inline` builds a fresh engine on every call; use it only where
that cost is acceptable.

The project script comes from a repository, so Nori loads it only for a
trusted directory. The sandbox below bounds what any script can do, but trust
is the gate.

## Vocabulary

Every clause takes a workflow first and returns a new one, so clauses chain:
inside `define-workflow`, with Steel's thread-first macro `~>` (or its alias
`->`), and in inline DSL. Workflow values are immutable.

### Defining workflows

| Form | Meaning |
| --- | --- |
| `(workflow "name")` | An empty workflow. The name may be a string or a symbol. |
| `(define-workflow name clause ...)` | Defines `name` as `(~> (workflow 'name) clause ...)` and registers it. |
| `(register-workflow w)` | Registers `w` under its name and returns it. Use it with `~>` chains. |
| `(default-workflow w)` | Registers `w` and makes it the session default. Also accepts a registered name. |
| `(workflow? x)` | Whether `x` is a workflow. |

### Clauses

| Clause | Meaning |
| --- | --- |
| `(turns n)` | The agent may run up to `n` prompt turns on its own before the user takes over. `0` stops after every turn. |
| `(on-change "prompt")` | Sent after a prompt turn that changed files. Alias: `hook-after-change`. |
| `(on-turn "prompt")` | Sent after every prompt turn. Alias: `hook-after-turn`. |
| `(on-commit "prompt")` | Sent after the agent creates a commit. Alias: `hook-after-commit`. |
| `(on-pr "prompt")` | Sent before the agent opens or updates a pull request. Alias: `hook-before-pr`. |
| `(on-stop "prompt")` | Sent before the agent hands control back to the user. Alias: `hook-before-stop`. |
| `(instruct "text")` | A standing instruction, given to the agent verbatim. |
| `(commit-style style option ...)` | What commits look like and when they are made. |
| `(commit-agent 'agent #:model "model")` | The agent, and optionally the model, that writes and batches the commits instead of the session's agent. |
| `(pr option ...)` | What a pull request contains and when work is upstreamed. |
| `(use w)` | Replaces everything but the name with `w`'s settings. `w` may also be a registered name, `(use 'land)`, which is how a workflow redefines itself on top of an earlier definition. |

Hooks accumulate; several hooks for one event are sent in order. `commit-style`
and `pr` merge: an option left out keeps its current value, so
`(pr #:max-lines 200)` tightens one limit of an inherited workflow.

`commit-style` takes an optional style, then options:

| Option | Value | Meaning |
| --- | --- | --- |
| style | `'conventional` or `'plain` | `type(scope): summary`, or a plain imperative summary. |
| `#:max-files` | integer, at least 1 | Most files one commit may touch. |
| `#:max-subject` | integer, at least 20 | Longest subject line, in characters. |
| `#:atomic` | `#t` or `#f` | One logical change per commit that builds and passes tests on its own. |
| `#:batch` | `'per-change`, `'per-turn`, or `'per-task` | Commit each change when complete, at the end of each turn that changed files, or once when the task is done. |
| `#:require-body` | `#t` or `#f` | Every commit body explains why. |

`pr` options:

| Option | Value | Meaning |
| --- | --- | --- |
| `#:title` | string | Title template, such as `"<type>(<scope>): <summary>"`. |
| `#:sections` | list of strings | Required description sections, in order. |
| `#:max-lines` | integer, at least 1 | Largest diff in changed lines before the work is split into several pull requests. |
| `#:max-files` | integer, at least 1 | Most files one pull request may touch. |
| `#:base` | string | The branch pull requests target. |
| `#:draft` | `#t` or `#f` | Open pull requests as drafts. |
| `#:upstream-when` | `'never`, `'ask`, `'tests-pass`, or `'done` | Never push; ask first; push and open the pull request once tests pass; or do so when the task is complete. |

An unknown option, a repeated option, or a value of the wrong type is an error
that names the form and shows the value it got.

## Example

```scheme
;; $NORI_HOME/init.scm
(define-workflow land
  (turns 3)
  (on-change "Commit what you just changed as focused commits; explain the why.")
  (on-stop "Run the tests that cover what you changed, and fix what fails.")
  (on-pr "Check that the description lists every user-visible change.")
  (commit-style 'conventional #:max-files 8 #:max-subject 72 #:atomic #t
                #:batch 'per-change #:require-body #t)
  (commit-agent 'claude #:model "haiku")
  (pr #:title "<type>(<scope>): <summary>"
      #:sections '("Why" "What" "Testing")
      #:max-lines 400
      #:base "main"
      #:upstream-when 'tests-pass))

;; The same style as a chain, like `.turns(1).hook_before_stop(...)`.
(define hotfix
  (~> (workflow "hotfix")
      (use land)
      (turns 1)
      (hook-before-stop "Say what you did not verify.")
      (pr #:max-lines 50 #:draft #t)))
(register-workflow hotfix)

(default-workflow land)
```

```scheme
;; <repo>/.nori/nori.scm: this repository lands smaller pull requests.
(define-workflow land (use 'land) (pr #:max-lines 200))
```

## Inline DSL

A prompt can adjust the session's workflow. Two spellings are recognized;
neither occurs in ordinary prose or code:

- A fenced block whose info string is exactly `nori`.
- A line that starts at column 0 with `,(`, outside any other fenced block.
  An indented `,(` and a `,(` inside a ```` ```scheme ```` block are left
  alone.

````text
Fix the flaky login test.
,(turns 5)
```nori
(on-stop "Run the login tests ten times.")
(use 'hotfix)
```
````

The DSL is removed from the prompt before it is sent. Inline forms are clauses
only: they apply, in order, to the default workflow (or to an empty workflow
named `prompt` when there is none), as if written inside
`(~> default-workflow ...)`. The definitions from the loaded scripts are in
scope, so `(use 'hotfix)` and helper functions such as
`(define (careful w) (~> w (on-stop "Run the tests.")))` work; a bare helper
name is a clause too (`careful`). Inline DSL cannot use `define`, `set!`,
`define-workflow`, `register-workflow`, or `default-workflow`, and it leaves
the loaded definitions unchanged.

## What the agent is told

A workflow renders as concise rules for the agent's first prompt, for example:

```text
Follow the "land" workflow:
- You may run up to 3 prompt turns on your own before the user takes over.
- Commit messages follow Conventional Commits: `type(scope): summary`. Keep the subject line within 72 characters. Every commit has a body that explains why the change was made.
- Keep commits atomic: one logical change per commit that builds and passes tests on its own. A commit touches at most 8 files. Commit each logical change as soon as it is complete.
- Do not run `git commit` yourself: Nori's commit agent, claude (haiku), writes and batches the commits by these rules.
- Pull request titles follow `<type>(<scope>): <summary>`. The description has these sections, in order: "Why", "What", "Testing". Keep each pull request within 400 changed lines; split larger work into a series of pull requests.
- Target the `main` branch. Once the tests pass, push the branch and open the pull request.
- After a turn that changes files, Nori will tell you: Commit what you just changed as focused commits; explain the why.
- Before you open a pull request, Nori will tell you: Check that the description lists every user-visible change.
- Before you stop, Nori will tell you: Run the tests that cover what you changed, and fix what fails.
```

Hooks are listed so the agent expects them. Nori still sends each hook's prompt
when its event happens.

## In a session

The harness (`nori-rs/harness/src/backend/workflow.rs`) applies the active
workflow:

- **Loading.** When a root session starts and a script exists, a
  `ScriptHost` loads it in the background; a prompt sent before it is ready
  runs without a workflow. A script error is shown once as a notice. Delegated
  children load nothing: their task carries what they need.
- **Telling.** The first user prompt after a workflow comes into force carries
  its rules in a leading `<workflow>` block, the same way goal context does.
  An inline clause that changes the workflow tells the agent again.
- **Inline DSL.** Nori removes the clauses from the prompt before the agent
  sees it. A prompt that was only clauses changes the workflow and sends
  nothing.
- **Hooks.** After a prompt turn that ended normally, Nori compares the working
  tree and `HEAD` with how they were when the user's turn started, and sends
  the first hook whose event happened, as a hidden turn wrapped in
  `<workflow_hook event="on-change">`, with an info line naming it. Without a
  snapshot from the turn's start (the scripts loaded during the turn) nothing
  counts as changed:
  - after a user turn: `on-change` (files changed), then `on-commit` (`HEAD`
    moved), then `on-turn`, then `on-stop`;
  - after a hook turn: only `on-commit` and `on-stop`, so a hook's own edits
    never start another round;
  - `on-stop` fires at most once per user turn; `on-pr` is never sent, because
    Nori cannot see a pull request coming, so the agent follows it from its
    rules.
- **Budget.** At most `turns` hook turns follow one user turn (3 when the
  workflow does not say). A hook is only sent when the session is idle; the
  goal loop continues after it.
- **Commit agent.** With `commit-agent`, the `on-change` work goes to that
  agent and model as a delegated task in the background, and the session's
  agent keeps going. It shows up as a subagent.

## Sandbox

Scripts may use the `nori/workflow` forms and pure Steel: arithmetic, strings,
lists, hash maps, `define`, `lambda`, `let`, `cond`, and the prelude's macros
such as `~>`. Nothing else is available:

- No files, processes, network, environment variables, or terminal I/O
  (`display` and `read-line` are denied too: the terminal belongs to the TUI).
- No `require`, `require-builtin`, `eval`, or `load`.
- No macro definitions (`define-syntax`, `syntax-rules`, `defmacro`). Steel
  expands macros inside its compiler, where the time budget cannot reach, so a
  self-referential macro would hang Nori or overflow its stack. Write a
  function instead: `(define (strict w) (~> w (turns 1)))`.
- Names starting with `%` or `#%`, and `Engine::...`, are Steel internals and
  are denied.
- No threads, sleeping, or blocking.
- A script is at most 256 KiB and nests forms at most 64 levels deep.
- Each file, and each inline block or line, runs for at most 500 ms
  (`nori_script::TIME_BUDGET`) before it is stopped.

The checks are layered:

1. Before Steel sees a script, a scan with Steel's own lexer rejects denied
   names, `require`, macro definitions, and oversized or deeply nested sources.
2. The engine also rebinds every denied global to a function that raises the
   same error.
3. The dangerous builtin modules are replaced with empty ones.
4. A watchdog interrupts evaluation past the time budget, and a panic inside
   Steel becomes an error.

The sandbox bounds time, not memory: a script that allocates without limit can
exhaust memory within its time budget. Do not load untrusted scripts; that is
why the project script requires a trusted directory.

## Errors

Every error names its origin (the file path, or `prompt`), a 1-based line and
column when Steel reports a position, and a message that names the form, for
example:

```text
/repo/.nori/nori.scm:2:3: turns: the turn count must be a whole number of at least 0, got 'many
```

Errors have a kind: `Io` (the file exists but cannot be read), `Syntax`,
`Sandbox`, `Eval` (unknown names, wrong arguments), or `Timeout`. A script
that fails to load contributes no workflows; Nori reports the error and
continues without them.
