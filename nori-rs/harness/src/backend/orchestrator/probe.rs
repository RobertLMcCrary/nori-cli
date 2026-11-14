//! Keeping the catalog current: installed agents are started briefly, in the background, to read
//! the models and settings they offer.
//!
//! A probe is a bare session: no hooks, no MCP servers, no transcript. Each agent has at most
//! one probe at a time, and an agent whose entry is fresh enough is left alone, so asking often
//! costs nothing.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;

use super::Seed;

/// How long an agent may take to start and open a session for its probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// Agents probed at once.
const PARALLEL_PROBES: usize = 3;

/// Agents with a probe running.
static PROBING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// Probe every installed agent whose catalog entry is older than `fresh_for`. Returns when they
/// have all answered or given up; the caller decides how long to wait for that.
pub(crate) async fn refresh(
    config: Arc<crate::config::NoriConfig>,
    cli_version: String,
    fresh_for: Duration,
) {
    let now = super::catalog::now_seconds();
    let permits = Arc::new(tokio::sync::Semaphore::new(PARALLEL_PROBES));
    let probes = crate::registry::list_available_agents()
        .into_iter()
        .filter(|agent| agent.is_installed)
        .map(|agent| agent.agent_name)
        .filter(|agent| {
            let refreshed_at = super::catalog::remembered(&config.nori_home, agent).refreshed_at;
            now - refreshed_at >= fresh_for.as_secs() as i64
        })
        .filter(|agent| {
            PROBING
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(agent.clone())
        })
        .map(|agent| {
            let config = Arc::clone(&config);
            let cli_version = cli_version.clone();
            let permits = Arc::clone(&permits);
            tokio::spawn(async move {
                if let Ok(_permit) = permits.acquire().await
                    && let Err(error) =
                        tokio::time::timeout(PROBE_TIMEOUT, probe(&config, cli_version, &agent))
                            .await
                            .unwrap_or_else(|_| Err("timed out".to_owned()))
                {
                    tracing::debug!(%agent, %error, "could not read what the agent offers");
                }
                PROBING
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&agent);
            })
        });
    futures::future::join_all(probes).await;
}

/// Start `agent` in a bare session, remember what it advertises, and stop it.
async fn probe(
    config: &crate::config::NoriConfig,
    cli_version: String,
    agent: &str,
) -> Result<(), String> {
    let mut bare = config.clone();
    bare.active_agent = agent.to_owned();
    bare.mcp_servers.clear();
    bare.session_start_hooks.clear();
    bare.session_end_hooks.clear();
    bare.async_session_start_hooks.clear();
    bare.async_session_end_hooks.clear();
    let bare = Arc::new(bare);
    let spec = crate::runtime::AgentPrepareSpec {
        config: Arc::clone(&bare),
        cli_version: cli_version.clone(),
        session_context: None,
        initial_context: None,
    };
    // As a child it keeps no transcript.
    let seed = Seed::root(Arc::clone(&bare), cli_version);
    let prepared = crate::runtime::prepare_delegated_agent(spec, &seed)
        .await
        .map_err(|error| format!("{error:#}"))?;
    let mut session = crate::runtime::launch_session(crate::runtime::SessionLaunchSpec {
        agent: prepared,
        start: crate::runtime::SessionStart::New,
    });
    let result = loop {
        match session.events.recv().await {
            Some(SessionEvent::Nori(NoriEvent::SessionStarted(started))) => {
                super::catalog::remember(&config.nori_home, agent, &started.config_options);
                break Ok(());
            }
            Some(SessionEvent::Nori(NoriEvent::SessionEnded(ended))) => {
                break Err(ended
                    .message
                    .unwrap_or_else(|| "the session ended".to_owned()));
            }
            Some(_) => {}
            None => break Err("the agent exited".to_owned()),
        }
    };
    let _ = session.handle.close_session().await;
    let _ = session.handle.shutdown().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A probe opens a bare session on the agent, reads what it advertises, and leaves no
    /// transcript behind.
    #[tokio::test]
    async fn a_probe_remembers_what_the_agent_offers() {
        let home = tempfile::tempdir().expect("home");
        let config = crate::config::NoriConfig {
            active_agent: "mock-model".to_owned(),
            cwd: home.path().to_path_buf(),
            nori_home: home.path().to_path_buf(),
            ..Default::default()
        };
        probe(&config, "probe-test".to_owned(), "mock-model-alt")
            .await
            .expect("probe the mock agent");
        let offer = super::super::catalog::remembered(home.path(), "mock-model-alt");
        let models: Vec<&str> = offer
            .models
            .iter()
            .map(|model| model.value.as_str())
            .collect();
        assert_eq!(models, ["mock-model-default", "mock-model-fast"]);
        assert!(offer.refreshed_at > 0);
        assert!(
            !home.path().join("transcripts").exists(),
            "a probe keeps no transcript"
        );
    }
}
