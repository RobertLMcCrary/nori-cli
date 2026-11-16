//! Starting the agents the user may switch to before they do.
//!
//! A built-in agent can start a session ahead of time (Claude: its CLI, which takes about a
//! second). Once the user's own session is up, every other installed built-in agent is asked to
//! do that for the same working directory and MCP servers, a few seconds later so the user's own
//! session starts undisturbed, and switching to one is then instant. The active agent keeps one
//! warm by itself after each session it opens.

use std::path::PathBuf;

use nori_protocol::acp::v1 as acp;

/// How long the user's session has to itself before other agents start warming up.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

/// In the background, have every other installed built-in agent start a session for `cwd`.
pub(super) fn others(current: String, cwd: PathBuf, mcp_servers: &[acp::McpServer]) {
    let mcp_servers = serde_json::to_value(mcp_servers).unwrap_or_default();
    tokio::spawn(async move {
        // The user's own session starts first, undisturbed.
        tokio::time::sleep(SETTLE).await;
        // Listing agents may look for executables on disk, so it stays off the async workers.
        drop(tokio::task::spawn_blocking(move || {
            super::host_tools::publish_catalog();
            for agent in crate::registry::list_available_agents() {
                if agent.agent_name == current || !agent.is_installed {
                    continue;
                }
                if let Ok(config) = crate::registry::get_agent_config(&agent.agent_name) {
                    nori_acp_host::in_process::prewarm(&config, cwd.clone(), mcp_servers.clone());
                }
            }
        }));
    });
}
