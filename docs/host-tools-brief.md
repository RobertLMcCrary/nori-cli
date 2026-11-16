# Brief: host tools, instant switching, workflows — done and open

Status: updated 2026-11-16 on branch `perf/instant-core`. The design that was planned here is
built; how it works now lives in the docs below. What follows is only what a next session needs.

## Done

- **Host tools** ([host-tools.md](host-tools.md)). Built-in Codex and Claude get Nori's tools as
  plain function calls (crate `nori-host-tools`): Codex as dynamic tools plus thread
  instructions, Claude as an in-process SDK server named `nori`. The `nori-client` MCP server
  is now only for external agents. Calls carry `_meta.nori.hostTool`, which the TUI recognizes
  by name. Verified live: native Claude ran `git_status`, `git_diff`, `git_log` and `git_commit`
  in a scratch repo and committed exactly one file.
- **New git tools** `git_diff` (per-file `+/−` cell) and `git_log` (history cell), read with gix.
- **Tool and prompt diet**, after reading t3code and Pi. t3code serves its own tools over MCP,
  so Nori's in-process path goes further than t3's. Descriptions are shorter and state what
  comes back. Each rule is said once, and goal instructions no longer name a server. Claude
  keeps the git, delegate, status and wait tools loaded; the rest are found by tool search.
- **Warm sessions** ([warm-sessions.md](warm-sessions.md)). Claude CLIs are started ahead of
  time, process-wide. Nori primes other built-in agents 5 s after a root session starts.
  Measurements are in `nori-rs/scripts/perf/README-switch.md`: CLI spawn to `initialize` is
  about 1.0 s, Nori `initialize` 10 ms, and `session/new` 16 ms.
- **Workflows** ([workflows.md](workflows.md)). A Steel Scheme DSL (crate `nori-script`)
  loaded Helix-style from `$NORI_HOME/init.scm` and, in trusted directories,
  `.nori/nori.scm`. It supports inline clauses in prompts, hooks (`on-change`, `on-commit`,
  `on-turn`, `on-stop`) sent as hidden turns within a turn budget, commit and PR rules told to
  the agent, and a designated commit agent that runs as a delegated task.
- **Ctrl+Space subagent switcher**, with fuzzy search, a live preview, and a focused
  follow view. Verified live (empty state).
- **Claude driver fixes**:
  - Tool results now reach the client. Every tool used to stay "in progress": `user` messages
    were dropped.
  - The client's MCP servers (stdio, HTTP, SSE) go to the CLI as they are. HTTP servers used
    to be dropped silently, and stdio ones were never listed.
  - `session/resume` keeps the session id.
  - New conversations get `--session-id`.
  - Agent errors show their reason instead of "Internal error" (Nori side).
- **Codex fixes**:
  - Generated images display again.
  - A dynamic tool call is always answered, so a turn never hangs on one.
  - The 21 translation tests run again (25/25).

## Still open

- **Verification**
  - Verified live in Rex against in-process Claude:
    - an `on-change` hook fired after an edit, and the agent committed by the
      workflow's rules;
    - parallel `git_status` and `git_log` finished with their data;
    - `/new` took a warm session ("Session ready" at once, against about 1 s cold).
  - Not yet run live: the commit agent (a delegated task), and the switch from Codex to a
    warm Claude, since there is no Codex login.
  - No Codex turn has been seen: the machine has no Codex login.
  - Toasts have never been seen on screen.
  - The user's Ctrl+Shift and "doesn't render to the bottom" reports were never reproduced.
  - `nix build .#nori` was still building at the last check. New crates must be tracked by
    git for the flake to see them.
- **Claude**
  - The CLI re-initializes MCP about 0.85 s after the first query, and the user's
    SessionStart hooks run on every spawn. Both sit before the first token, even on a warm
    session.
  - `rate_limit_event` is dropped inside the SDK; surfacing it as a toast needs an SDK change
    or a raw stream hook.
  - A fork does not pass `--session-id`, so a forked conversation's id differs from the
    driver's. Check whether the CLI accepts `--session-id` with `--fork-session`.
  - The account's `cli_path` is not filled in from the active account automatically.
  - Kill the process group on drop (no reaper in-process).
- **Git, PR and CI**
  - There is no `ci_status` tool and no forge client. A plain HTTP client to the forge's
    API is needed; no shelling to `gh`.
  - `on-pr` cannot be detected, so the agent follows that rule from its instructions alone.
- **Code debts**
  - Codex `guardian_action_summary` has a catch-all arm.
  - Codex `ApprovedMcpPolicyAmendment` has a placeholder label.
  - `nori-skillsets` and `nori-ai` are still shelled out.
  - The dev binary's `__eh_frame` linker warning remains.
- **Environment**
  - Tests that fail on this machine for reasons outside the code:
    - `harness shell::tests::{detects_sh, detects_zsh, fish_fallback_to_zsh}` (Nix zsh path),
    - `tui remote_control::tests::disable_disconnects_the_controller_and_stops_every_listener`,
    - `acp-host remote_ws one_server_listens_on_loopback_...`, which hangs.
  - The default `cargo` here is a `mitts` shim that breaks on renamed dependencies; use
    `~/.local/share/cargo/bin/cargo`.
- **Housekeeping**: decide whether to commit or delete `PERF_PLAN.md` (untracked, repo root).
