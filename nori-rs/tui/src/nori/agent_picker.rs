//! Agent picker component for ACP mode.
//!
//! This module provides the UI for selecting between available ACP agents.
//! Selecting an agent immediately starts preparation of a live candidate.

use nori_harness::AcpAgentInfo;
use nori_harness::list_available_agents;
use nori_tui_components::KeyHint;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::SelectionAction;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;

/// Create selection view parameters for the agent picker.
///
/// # Arguments
/// * `current_agent` - The currently active agent name
/// * `app_event_tx` - The app event sender for triggering selection events
pub fn agent_picker_params(
    current_agent: &str,
    app_event_tx: AppEventSender,
    recording_enabled: bool,
) -> SelectionViewParams {
    agent_picker_params_with_subtitle(
        current_agent,
        app_event_tx,
        recording_enabled,
        "Creates new conversation with selected agent (history not preserved)".to_string(),
    )
}

pub fn agent_picker_params_with_subtitle(
    current_agent: &str,
    _app_event_tx: AppEventSender,
    recording_enabled: bool,
    subtitle: String,
) -> SelectionViewParams {
    let available_agents = list_available_agents();
    let current_normalized = current_agent.to_lowercase();

    let items: Vec<SelectionItem> = available_agents
        .into_iter()
        .map(|agent| {
            let is_current = agent.agent_name.to_lowercase() == current_normalized;
            let agent_name = agent.agent_name.clone();
            let highlight_agent = agent_name.clone();
            let highlight_display = agent.display_name.clone();

            // Start preparing the selected agent as soon as the picker closes.
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                tx.send(AppEvent::PrepareAgentCandidate {
                    agent_name: agent_name.clone(),
                });
            })];

            // Resting on a row starts its agent warming, so choosing it is nearly instant.
            let on_highlight: Option<SelectionAction> = (!is_current).then(|| {
                Box::new(move |tx: &AppEventSender| {
                    tx.send(AppEvent::HighlightAgentCandidate {
                        agent_name: highlight_agent.clone(),
                        display_name: highlight_display.clone(),
                    });
                }) as SelectionAction
            });

            SelectionItem {
                name: agent.display_name,
                description: Some(agent.description),
                is_current,
                actions,
                on_highlight,
                dismiss_on_select: true,
                ..Default::default()
            }
        })
        .collect();

    SelectionViewParams {
        title: Some("Select agent".to_string()),
        subtitle: Some(subtitle),
        footer_hint: Some(standard_popup_hint_line()),
        footer_hint_right: Some(recording_footer_hint(recording_enabled)),
        picker_footer_hints: Some(vec![
            KeyHint::new("↑↓/j/k", "move"),
            KeyHint::new("enter", "select"),
            KeyHint::new(
                "shift-tab",
                if recording_enabled {
                    "rec off"
                } else {
                    "rec on"
                },
            ),
            KeyHint::new("esc", "close"),
        ]),
        on_shift_tab: Some(Box::new(move |tx| {
            tx.send(AppEvent::SetConfigAcpWireRecording(!recording_enabled));
        })),
        // Closing the picker without a choice drops whatever it warmed.
        on_dismiss: Some(Box::new(|tx| {
            tx.send(AppEvent::DiscardWarmAgentCandidate);
        })),
        items,
        ..Default::default()
    }
    .picker()
}

fn recording_footer_hint(enabled: bool) -> Line<'static> {
    let (symbol, status, action) = if enabled {
        ("●", "on", "disable")
    } else {
        ("○", "off", "enable")
    };
    let symbol_span: Span<'static> = if enabled { symbol.red() } else { symbol.into() };

    Line::from(vec![
        "Recording: ".dim(),
        symbol_span,
        format!(" {status}  Shift-Tab to {action}").dim(),
    ])
}

/// Create selection view parameters for the model picker when the agent has not advertised a
/// model setting.
pub fn acp_model_picker_params() -> SelectionViewParams {
    let items: Vec<SelectionItem> = vec![SelectionItem {
        name: "No model options".to_string(),
        description: Some("The agent did not advertise a model setting.".to_string()),
        is_current: false,
        actions: vec![],
        dismiss_on_select: true,
        ..Default::default()
    }];

    SelectionViewParams {
        title: Some("Select model".to_string()),
        subtitle: Some("This agent doesn't offer model switching".to_string()),
        footer_hint: Some(Line::from("Press esc to dismiss.")),
        items,
        ..Default::default()
    }
    .picker()
}

/// Create selection view parameters for `/model` before a session exists, when there is no
/// agent configuration to read yet.
pub fn acp_model_picker_no_session_params() -> SelectionViewParams {
    let items: Vec<SelectionItem> = vec![SelectionItem {
        name: "No active session yet".to_string(),
        description: Some("Send a message, run /new, or /login first.".to_string()),
        is_current: false,
        actions: vec![],
        dismiss_on_select: true,
        ..Default::default()
    }];

    SelectionViewParams {
        title: Some("Select model".to_string()),
        subtitle: Some("Start a session to choose a model".to_string()),
        footer_hint: Some(Line::from("Press esc to dismiss.")),
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
    use ratatui::style::Color;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn test_agent_picker_params_lists_available_agents() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);

        let params = agent_picker_params("mock-model", tx, false);

        assert!(params.title.is_some());
        assert!(params.title.unwrap().contains("Select agent"));
        assert!(!params.items.is_empty());

        // Should have mock-model as current
        let mock_agent = params.items.iter().find(|i| i.name == "Mock ACP");
        assert!(mock_agent.is_some());
        assert!(mock_agent.unwrap().is_current);
    }

    #[test]
    fn recovery_agent_picker_uses_spawn_failure_as_subtitle() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);

        let params = agent_picker_params_with_subtitle(
            "nonexistent",
            tx,
            false,
            "Agent 'nonexistent' is not registered".to_string(),
        );

        assert_eq!(
            params.subtitle.as_deref(),
            Some("Agent 'nonexistent' is not registered")
        );
    }

    #[test]
    fn agent_picker_footer_shows_recording_disabled_and_shift_tab_toggles_on() {
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);

        let params = agent_picker_params("mock-model", tx.clone(), false);

        let footer = params.footer_hint_right.as_ref().expect("recording footer");
        assert_eq!(footer.to_string(), "Recording: ○ off  Shift-Tab to enable");

        let action = params.on_shift_tab.as_ref().expect("shift-tab action");
        action(&tx);
        let event = rx.try_recv().expect("shift-tab event");
        assert!(
            matches!(event, AppEvent::SetConfigAcpWireRecording(true)),
            "expected SetConfigAcpWireRecording(true), got: {event:?}"
        );
    }

    #[test]
    fn recording_footer_uses_red_filled_dot_when_enabled() {
        let footer = recording_footer_hint(true);
        assert_eq!(footer.to_string(), "Recording: ● on  Shift-Tab to disable");
        let dot = footer
            .spans
            .iter()
            .find(|span| span.content == "●")
            .expect("filled recording dot");
        assert_eq!(dot.style.fg, Some(Color::Red));
    }

    #[test]
    fn test_acp_model_picker_explains_missing_model_option() {
        let params = acp_model_picker_params();

        assert!(params.title.is_some());
        assert_eq!(
            params.subtitle.as_deref(),
            Some("This agent doesn't offer model switching")
        );
        assert_eq!(params.items[0].name, "No model options");
    }

    #[test]
    fn test_acp_model_picker_without_session_points_at_starting_one() {
        let params = acp_model_picker_no_session_params();

        assert_eq!(params.title.as_deref(), Some("Select model"));
        assert_eq!(
            params.subtitle.as_deref(),
            Some("Start a session to choose a model")
        );
        assert_eq!(params.items[0].name, "No active session yet");
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
