# Forge: pull requests and CI

Nori talks to the forge a repository lives on, so an **agent** can see how its
branch stands in CI and open its pull request without a shell, and a
**workflow** can react to CI. Only GitHub is supported: github.com, and a
GitHub Enterprise host that `GH_HOST` names or `gh` is logged in to. The client
(crate `nori-forge`) calls the REST API over HTTPS; nothing shells out to `gh`.

## Which repository, which token

- The branch is the checked-out one. Its remote is the branch's `pushRemote`,
  else its `remote`, else `origin`, and the remote's URL names the repository.
- The token is `GH_TOKEN` or `GITHUB_TOKEN` (`GH_ENTERPRISE_TOKEN` or
  `GITHUB_ENTERPRISE_TOKEN` for an Enterprise host), else the `oauth_token`
  in `gh`'s `hosts.yml`. A `gh` that keeps its token in the system keychain
  leaves none there; set `GH_TOKEN` then.
- Without a token, a public repository still answers reads, within GitHub's
  anonymous limit of 60 requests an hour. Opening a pull request and reading a
  job's log need a token.
- Reads are conditional: the client keeps each answer's `ETag` and asks again
  with `If-None-Match`, so re-reading unchanged checks costs no rate limit.
  One client per remote URL lives for the whole process.

## Tools

Both tools are served like the git tools (see [host-tools.md](host-tools.md)
and [nori-client-mcp.md](nori-client-mcp.md)).

**`ci_status`** `{ branch?, logs? }` answers how the branch stands on GitHub:
its pull request (open first, else the latest), the commit GitHub has for the
branch, whether that is `HEAD` (`head_pushed`), and the checks on it. Check
runs and the older commit statuses count together. Passed checks are counted;
failed, running and cancelled ones are listed by name with a link. With
`logs`, each of the first three failed GitHub Actions jobs carries the end of
its log: the 40 lines up to its last `##[error]`, without timestamps or colors.
A failure decides the state even while other checks run.

**`pull_request`** `{ title, body, base?, draft? }` opens a pull request from
the checked-out branch, or updates the open one's title and description. In
order, it:

1. Refuses when the workflow's `#:upstream-when` is `'never`.
2. Requires that GitHub has the branch at `HEAD`, and otherwise names the
   `git push` to run. Pushing is the agent's: `gix` cannot push.
3. Holds once per commit for the workflow's `on-pr` prompts: the first call
   at a commit answers `{"opened": false, "before_opening": ...}`, and the
   next call at that commit goes on.
4. Picks the base: `base`, else the workflow's `#:base`, else the remote's
   default branch, else `main`.
5. Checks the workflow's rules and lists every one broken: each of
   `#:sections` must be a heading in the body (`## Why`, `**Why**` and `Why:`
   all count), and the commits since the merge base with the remote's base
   must stay within `#:max-lines` changed lines and `#:max-files` files.
6. Asks the user in the conversation unless the user has already answered:
   `'tests-pass` and `'done` allow it, `'ask` always asks, and without a rule
   Nori asks unless the session's approval policy is `never`.
7. Opens it, as a draft when `draft` or the workflow's `#:draft` says so.

## CI hooks

A workflow with an `on-ci-fail` or `on-ci-pass` prompt has Nori watch CI.
After a turn, when the branch's remote-tracking ref has moved since the user's
turn started (the agent pushed), Nori watches the checks on the pushed commit:

- It reads them after 10 s, 15 s and 30 s, then every minute.
- A newer push replaces the watch. Cancelled checks end it, as do no checks
  three minutes into the watch, or checks still undecided after 45 minutes.
- When the checks fail, Nori sends the `on-ci-fail` prompt after a report
  naming each failed check, its link, and the end of the first three failed
  Actions jobs' logs. When all pass, it sends `on-ci-pass`.
- The prompt goes as a hidden hook turn when the session is idle, and counts
  against the workflow's turn budget. A busy session gets it when its turn
  ends. Past the budget, or without a prompt for that verdict, only the user
  is told.

## In the TUI

`ci_status` renders as `◉ CI  <branch>  ✗ 1 failed · 7 passed`, with the pull
request and each failed or running check under it, and a failed check's
first error line beside its name. `pull_request` renders as
`⇡ Opened #413 <title>`, with `head → base` and the link, or as the
workflow's word before opening. A refusal keeps each broken rule on its own
line.
