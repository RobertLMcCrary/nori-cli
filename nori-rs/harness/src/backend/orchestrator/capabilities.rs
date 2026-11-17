//! What `agent_capabilities` answers: the agents a session can delegate to, with what each offers.

use std::sync::atomic::Ordering;

use serde::Serialize;

use super::Inner;

/// The agents a session can delegate to, for `agent_capabilities`.
#[derive(Debug, Serialize)]
pub(super) struct Capabilities {
    pub(super) can_delegate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reason: Option<String>,
    pub(super) depth: i64,
    pub(super) running_tasks: i64,
    pub(super) agents: Vec<AgentEntry>,
}

#[derive(Debug, Serialize)]
pub(super) struct AgentEntry {
    /// The value to pass as `agent`.
    pub(super) agent: String,
    pub(super) name: String,
    /// The agent this session runs on.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) is_yours: bool,
    /// The model a child gets when `model` is not given, when Nori is configured with one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) default_model: Option<String>,
    /// Models the agent offered when Nori last ran it; empty when it has not run yet. Any model
    /// it offers can be passed; an unknown one fails with the list.
    pub(super) models: Vec<ModelEntry>,
    /// How long ago, in seconds, Nori last read what the agent offers; absent when it never has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) read_seconds_ago: Option<i64>,
    /// The agent's other settings that `options` can set, with their values.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) settings: Vec<SettingEntry>,
}

#[derive(Debug, Serialize)]
pub(super) struct ModelEntry {
    pub(super) model: String,
    pub(super) name: String,
}

#[derive(Debug, Serialize)]
pub(super) struct SettingEntry {
    pub(super) setting: String,
    pub(super) name: String,
    pub(super) values: Vec<String>,
}

impl Inner {
    pub(super) fn capabilities(&self) -> Capabilities {
        let config = &self.seed.config;
        let agents = crate::registry::list_available_agents()
            .into_iter()
            .filter(|agent| agent.is_installed)
            .map(|agent| AgentEntry {
                models: offered_models(&config.nori_home, &agent.agent_name),
                read_seconds_ago: Some(
                    super::catalog::remembered(&config.nori_home, &agent.agent_name).refreshed_at,
                )
                .filter(|refreshed_at| *refreshed_at > 0)
                .map(|refreshed_at| super::catalog::now_seconds() - refreshed_at),
                settings: super::catalog::remembered(&config.nori_home, &agent.agent_name)
                    .settings
                    .into_iter()
                    .map(|setting| SettingEntry {
                        setting: setting.id,
                        name: setting.name,
                        values: setting
                            .values
                            .into_iter()
                            .map(|value| value.value)
                            .collect(),
                    })
                    .collect(),
                default_model: config.default_models.get(&agent.agent_name).cloned(),
                is_yours: agent.agent_name == config.active_agent,
                name: agent.display_name,
                agent: agent.agent_name,
            })
            .collect();
        Capabilities {
            can_delegate: true,
            reason: None,
            depth: self.seed.depth,
            running_tasks: self.seed.tree.running.load(Ordering::Acquire),
            agents,
        }
    }
}

/// The models `agent` offered when it last ran, then any others Nori knows it can be given.
fn offered_models(home: &std::path::Path, agent: &str) -> Vec<ModelEntry> {
    let mut models: Vec<ModelEntry> = super::catalog::remembered(home, agent)
        .models
        .into_iter()
        .map(|model| ModelEntry {
            model: model.value,
            name: model.name,
        })
        .collect();
    let known = crate::registry::AgentKind::from_slug(agent)
        .map(crate::registry::AgentKind::other_models)
        .unwrap_or_default();
    for other in known {
        if !models.iter().any(|model| model.model == other.id) {
            models.push(ModelEntry {
                model: other.id.to_owned(),
                name: other.label.to_owned(),
            });
        }
    }
    models
}
