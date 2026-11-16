//! Terminal API module
//!
//! Implements Client-side PTY approach where the Agent sends terminal requests
//! to the Client (editor), and the Client manages the actual PTY.
//!
//! This matches the TypeScript claude-code-acp implementation's terminal API:
//! - `terminal/create`: Create a new terminal and execute a command
//! - `terminal/output`: Get current output and status
//! - `terminal/wait_for_exit`: Wait for command to complete
//! - `terminal/kill`: Kill the command (terminal remains valid)
//! - `terminal/release`: Release terminal resources

mod client;
mod handle;

pub use client::TerminalClient;
pub use handle::TerminalHandle;

// Re-export relevant types from sacp::schema for convenience
pub use sacp::schema::CreateTerminalRequest;
pub use sacp::schema::CreateTerminalResponse;
pub use sacp::schema::KillTerminalRequest;
pub use sacp::schema::KillTerminalResponse;
pub use sacp::schema::ReleaseTerminalRequest;
pub use sacp::schema::ReleaseTerminalResponse;
pub use sacp::schema::TerminalExitStatus;
pub use sacp::schema::TerminalId;
pub use sacp::schema::TerminalOutputRequest;
pub use sacp::schema::TerminalOutputResponse;
pub use sacp::schema::WaitForTerminalExitRequest;
pub use sacp::schema::WaitForTerminalExitResponse;
