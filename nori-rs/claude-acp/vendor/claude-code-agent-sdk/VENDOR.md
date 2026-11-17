# Vendored Claude Code Agent SDK

This directory vendors `claude-code-agent-sdk` 0.1.39 from
https://github.com/soddygo/claude-code-agent-sdk under the upstream MIT license
(the complete license text is in `LICENSE`). The upstream Rust SDK does not yet
model Claude Code's `rate_limit_event` messages and drops raw JSON when parsing
fails. The local delta adds a raw-preserving `RateLimitEvent` message variant
and retains the original JSON in `MessageParseError::data`. It also parses
result costs when another workspace dependency enables serde_json's
`arbitrary_precision` feature.

The crate is patched locally by the `nori-rs` workspace to keep adapter behavior
and these compatibility fixes reproducible without changing unrelated SDK
versions.
