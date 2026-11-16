//! Public types for Claude Code ACP Agent
//!
//! This module contains all the shared types used across the crate.

mod config;
mod error;
mod meta;
mod session;
mod tool;

pub use config::AgentConfig;
pub use error::AgentError;
pub use error::ErrorCode;
pub use error::Result;
pub use meta::ClaudeCodeMeta;
pub use meta::ClaudeCodeOptions;
pub use meta::NewSessionMeta;
pub use meta::SystemPromptMeta;
pub use session::SessionStats;
pub use session::TokenUsage;
pub use tool::ToolCallLocation;
pub use tool::ToolInfo;
pub use tool::ToolInfoContent;
pub use tool::ToolKind;
pub use tool::ToolUseEntry;
pub use tool::ToolUseType;
