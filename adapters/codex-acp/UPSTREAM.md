# codex-acp (vendored)

Vendored from https://github.com/zed-industries/codex-acp at commit `296069e841634cd4bb9bc4515602d836e49231ec` (v0.16.0, archived 2026-07).
The upstream repository is archived; this copy is maintained here. Licensed Apache-2.0 (see LICENSE).

It is its own Cargo workspace on purpose: it depends on the upstream `openai/codex` crates by git tag,
which cannot live in the same dependency graph as Nori's own forked `codex-*` crates.

## Local changes
See `git log -- adapters/codex-acp`.
