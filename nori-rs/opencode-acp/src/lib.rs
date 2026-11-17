//! OpenCode v2 API support for Nori's in-process ACP adapter.
//!
//! OpenCode's execution engine remains an external process. This crate owns its HTTP
//! transport, event stream, and ACP translation. See the turn-lifecycle documentation for
//! remaining differences from OpenCode's upstream ACP adapter.

mod agent;
mod config;
mod directories;
mod event;
mod lifecycle;
mod mcp;
mod nesting_defaults;
mod permission;
mod provider_error;
mod question;
mod replay;
mod server;
mod turn;

pub use agent::serve_over_pipe;
pub use server::OpenCodeServer;
