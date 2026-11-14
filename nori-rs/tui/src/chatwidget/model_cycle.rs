//! Next and previous model (Ctrl+N / Ctrl+P by default), without opening the picker.
//!
//! A press shows the new model at once: the agent's advertised configuration is updated in
//! place, so the footer changes on the keypress and the agent's own echo of the change finds
//! nothing new to report. The agent is told in the background, one request at a time: presses
//! that come while a request is out only move the target, which is sent when the request
//! returns, so a quick run of presses costs two requests at most. If the agent refuses, the
//! configuration from before the run comes back.

use nori_protocol::acp::v1 as acp;

use super::ChatWidget;
use crate::nori::session_config_picker::ConfigPickerOrigin;

/// Which way a press steps through the agent's models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelStep {
    Next,
    Previous,
}

/// A run of model presses still being applied.
#[derive(Debug, Default)]
pub(crate) struct ModelCycle {
    /// A request is out; its result settles it.
    in_flight: bool,
    /// Where the presses since the request went out point.
    queued: Option<Target>,
    /// The configuration before the run's first press, restored if the agent refuses.
    before: Option<Vec<acp::SessionConfigOption>>,
}

#[derive(Debug, Clone)]
struct Target {
    config_id: String,
    value: String,
    option_name: String,
    value_name: String,
}

impl ChatWidget {
    pub(crate) fn cycle_model(&mut self, step: ModelStep) {
        let mut options = self.agent_config().raw_options();
        let Some(option) = options
            .iter_mut()
            .find(|option| option.category == Some(acp::SessionConfigOptionCategory::Model))
        else {
            self.add_info_message(
                "This agent doesn't offer a choice of model.".to_owned(),
                None,
            );
            return;
        };
        let option_name = option.name.clone();
        let config_id = option.id.to_string();
        let acp::SessionConfigKind::Select(select) = &mut option.kind else {
            return;
        };
        let offered: Vec<(String, String)> = match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(values) => values
                .iter()
                .map(|value| (value.value.to_string(), value.name.clone()))
                .collect(),
            acp::SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| group.options.iter())
                .map(|value| (value.value.to_string(), value.name.clone()))
                .collect(),
            // The schema's option layouts are not exhaustive; an unknown one offers nothing
            // that can be stepped through.
            _ => Vec::new(),
        };
        if offered.len() < 2 {
            self.add_info_message("This agent offers one model.".to_owned(), None);
            return;
        }
        let current = select.current_value.to_string();
        let position = offered
            .iter()
            .position(|(value, _)| *value == current)
            .unwrap_or(0);
        let next = match step {
            ModelStep::Next => (position + 1) % offered.len(),
            ModelStep::Previous => (position + offered.len() - 1) % offered.len(),
        };
        let (value, value_name) = offered[next].clone();
        select.current_value = value.clone().into();

        if self.model_cycle.before.is_none() {
            self.model_cycle.before = Some(self.agent_config().raw_options());
        }
        self.sync_acp_session_config_snapshot(&options);
        let target = Target {
            config_id,
            value,
            option_name,
            value_name,
        };
        if self.model_cycle.in_flight {
            self.model_cycle.queued = Some(target);
        } else {
            self.send_model_target(target);
        }
        self.request_redraw();
    }

    fn send_model_target(&mut self, target: Target) {
        self.model_cycle.in_flight = true;
        self.set_acp_session_config_option(
            target.config_id,
            target.value,
            target.option_name,
            target.value_name,
            false,
            ConfigPickerOrigin::Cycle,
        );
    }

    /// The agent answered a model switch made by a press, with the configuration it now has.
    /// While more presses are queued their target stands, so the answer is not shown.
    pub(crate) fn on_model_cycle_result(
        &mut self,
        result: Result<Option<Vec<acp::SessionConfigOption>>, String>,
    ) {
        if let Some(target) = self.model_cycle.queued.take() {
            self.send_model_target(target);
            return;
        }
        self.model_cycle.in_flight = false;
        let before = self.model_cycle.before.take();
        match result {
            Ok(Some(options)) => self.sync_acp_session_config_snapshot(&options),
            Ok(None) => {}
            Err(error) => {
                if let Some(before) = before {
                    self.sync_acp_session_config_snapshot(&before);
                }
                self.add_info_message(format!("Couldn't switch models: {error}"), None);
            }
        }
        self.request_redraw();
    }
}
