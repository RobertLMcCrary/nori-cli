# Nori CLI Docs

One prescriptive document per cross-cutting topic. Crate-level details live in
each crate's `docs.md`.

| Doc | Topic |
| --- | --- |
| [glossary.md](glossary.md) | Canonical terms for actors, turns, sessions, and tools |
| [turn-lifecycle.md](turn-lifecycle.md) | Turn state, `session/update` routing, and cancellation |
| [remote-control.md](remote-control.md) | Remote ACP transport behind `--remote` and `/remote-control` |
| [nori-client-mcp.md](nori-client-mcp.md) | The backend-owned `nori-client` MCP server and goals |
| [host-tools.md](host-tools.md) | Nori's tools handed straight to built-in agents, with no MCP server |
| [forge.md](forge.md) | GitHub pull requests and CI: `ci_status`, `pull_request`, and CI hooks |
| [warm-sessions.md](warm-sessions.md) | Agent sessions started ahead of time so switching is instant |
| [headless.md](headless.md) | `nori exec` plaintext mode and the ACP stdio facade |
| [transcripts.md](transcripts.md) | Transcript file locations and schema |
| [workflows.md](workflows.md) | The `nori/workflow` Scheme DSL: scripts, inline prompts, and the sandbox |

Rules for these docs:

- Describe current behavior in prescriptive terms. No status lines, history,
  implementation plans, or investigation logs; git history keeps those.
- A PR that changes behavior covered here updates the doc in the same PR.
- Add a new doc only for a topic that crosses crate boundaries and is easy to
  get wrong. Everything else belongs in the owning crate's `docs.md`.
