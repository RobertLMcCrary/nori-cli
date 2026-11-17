//! In-process ACP adapter backed by the official Gemini Core execution engine.
mod agent;
mod auth;
mod config;
mod engine;
mod mcp;
mod tool_output;
mod turn;
pub use agent::serve_over_pipe;
