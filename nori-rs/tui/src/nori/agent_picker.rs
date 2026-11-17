//! Provider connections and previously advertised model choices.

use nori_harness::AcpAgentInfo;
use nori_harness::list_available_agents;

use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;

/// Provider connections; authentication status is not exposed uniformly by ACP agents.
pub(crate) fn provider_picker_params() -> SelectionViewParams {
    let items = list_available_agents()
        .into_iter()
        .map(|provider| {
            let agent_name = provider.agent_name;
            let availability = if provider.is_installed {
                "Available"
            } else {
                "Setup required"
            };
            SelectionItem {
                name: provider.display_name,
                description: Some(format!(
                    "{availability} · sign-in status unknown · select to connect"
                )),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::LoginProvider {
                        agent_name: agent_name.clone(),
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            }
        })
        .collect();
    SelectionViewParams {
        title: Some("Providers".to_string()),
        subtitle: Some("Connect a provider to use its models".to_string()),
        is_searchable: true,
        items,
        ..Default::default()
    }
    .picker()
}

/// Get information about an agent by agent name
#[allow(dead_code)]
pub fn get_agent_info(agent_name: &str) -> Option<AcpAgentInfo> {
    let normalized = agent_name.to_lowercase();
    list_available_agents()
        .into_iter()
        .find(|agent| agent.agent_name.to_lowercase() == normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_event::AppEvent;
    use crate::app_event_sender::AppEventSender;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn provider_picker_connects_the_selected_provider() {
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let params = provider_picker_params();
        let provider = params
            .items
            .iter()
            .find(|item| item.name == "Mock ACP")
            .expect("mock provider");
        assert!(
            provider
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("sign-in status unknown")
        );
        provider.actions[0](&tx);
        assert!(
            matches!(rx.try_recv(), Ok(AppEvent::LoginProvider { agent_name }) if agent_name == "mock-model")
        );
    }

    #[test]
    fn test_get_agent_info() {
        let info = get_agent_info("mock-model");
        assert!(info.is_some());
        assert_eq!(info.unwrap().display_name, "Mock ACP");

        let info = get_agent_info("Mock-Model"); // Case insensitive
        assert!(info.is_some());

        let info = get_agent_info("unknown-agent");
        assert!(info.is_none());
    }
}
