//! ACP Agent implementation
//!
//! This module provides the core Claude ACP Agent that handles:
//! - ACP protocol requests (initialize, session/new, session/prompt, etc.)
//! - Session lifecycle management
//! - Message conversion between ACP and Claude SDK

mod core;
mod flush;
mod handlers;
mod runner;
mod slash_commands;

pub use core::ClaudeAcpAgent;
pub use runner::{InProcessOptions, run_acp, run_acp_with_cli, serve_in_process, shutdown_otel};
