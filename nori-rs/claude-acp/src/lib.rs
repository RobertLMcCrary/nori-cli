//! Claude Code ACP Agent
//!
//! A Rust implementation of the ACP (Agent Client Protocol) Agent for Claude Code,
//! enabling editors like Zed to use Claude Code capabilities.
//!
//! ## Features
//!
//! - ACP protocol support over stdio
//! - Session management with token usage tracking
//! - Streaming responses
//! - Permission mode handling
//!
//! ## Quick Start
//!
//! ```no_run
//! use claude_code_acp::run_acp;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     run_acp().await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Environment Variables
//!
//! - `ANTHROPIC_BASE_URL`: Custom API base URL
//! - `ANTHROPIC_API_KEY`: API key (preferred)
//! - `ANTHROPIC_AUTH_TOKEN`: Auth token (legacy, fallback)
//! - `ANTHROPIC_MODEL`: Model to use (default: the Claude CLI's own choice)
//! - `ANTHROPIC_SMALL_FAST_MODEL`: Model for fast operations
//! - `MAX_THINKING_TOKENS`: Maximum tokens for extended thinking mode
//!
//! ## Configuration Loading Priority
//!
//! The agent loads configuration from multiple sources with the following priority (highest to lowest):
//!
//! 1. **Environment Variables** - Override all other sources
//! 2. **Settings Files - Top-level fields** - Used when environment variables are not set
//! 3. **Settings Files - `env` object** - Fallback compatible with Claude Code CLI format
//! 4. **Defaults** - Fallback values
//!
//! Settings files are loaded from:
//! - `~/.claude/settings.json` (user settings)
//! - `.claude/settings.json` (project settings)
//! - `.claude/settings.local.json` (local settings, highest priority among settings files)
//!
//! ### Example settings.json
//!
//! Using top-level fields:
//! ```json
//! {
//!   "model": "claude-opus-4-20250514",
//!   "smallFastModel": "claude-haiku-4-20250514",
//!   "apiBaseUrl": "https://api.anthropic.com"
//! }
//! ```
//!
//! Using `env` object (compatible with Claude Code CLI):
//! ```json
//! {
//!   "env": {
//!     "ANTHROPIC_MODEL": "claude-opus-4-20250514",
//!     "ANTHROPIC_SMALL_FAST_MODEL": "claude-haiku-4-20250514",
//!     "ANTHROPIC_BASE_URL": "https://api.anthropic.com"
//!   }
//! }
//! ```

pub mod agent;
pub mod cli;
pub mod command_safety;
pub mod converter;
pub mod hooks;
pub mod mcp;
pub mod permissions;
pub mod session;
pub mod settings;
pub mod terminal;
pub mod tracing;
pub mod types;
pub mod utils;

pub use agent::InProcessOptions;
pub use agent::prewarm;
pub use agent::run_acp;
pub use agent::run_acp_with_cli;
pub use agent::serve_in_process;
pub use agent::shutdown_otel;
pub use cli::Cli;
pub use hooks::HookCallbackRegistry;
pub use hooks::create_post_tool_use_hook;
pub use hooks::create_pre_tool_use_hook;
pub use mcp::AcpMcpServer;
pub use mcp::McpServer;
pub use mcp::ToolContext;
pub use mcp::ToolRegistry;
pub use mcp::ToolResult;
pub use mcp::get_disallowed_tools;
pub use settings::Settings;
pub use settings::SettingsManager;
pub use terminal::TerminalClient;
pub use terminal::TerminalHandle;
pub use types::AgentConfig;
pub use types::AgentError;
pub use types::NewSessionMeta;
pub use types::Result;
