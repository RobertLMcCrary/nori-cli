//! Agent configuration state and status assembly.
//!
//! One place owns the agent's advertised ACP configuration: it is updated from
//! config-option updates, explicit snapshots, and successful mutations, and it
//! feeds `/config`, the footer's mode label, the configuration history lines,
//! and both status renderings. The status views themselves are pure, so every
//! value they show is assembled here.

use super::*;

use crate::nori::agent_config_state::AgentConfigState;
use crate::nori::session_header::AgentStatus;
use crate::nori::session_header::AgentStatusHandle;
use crate::nori::session_header::SkillsetStatus;
use crate::nori::session_header::StatusViewModel;

/// What the footer's model segment says: the agent, the model with the effort it runs at (what
/// Ctrl+N, Ctrl+P and Shift+Space change), and any other option the agent has moved off its
/// default. The mode has its own footer label.
fn footer_label(agent: &str, config: &AgentConfigState) -> Option<String> {
    use nori_protocol::acp::v1::SessionConfigOptionCategory as Category;

    let mut parts: Vec<String> = Vec::new();
    if !agent.is_empty() {
        parts.push(agent.to_owned());
    }
    if let Some(model) = config.option_for_category(&Category::Model) {
        parts.push(model.display_value());
    }
    let effort = config.effort_option();
    if let Some(effort) = effort {
        parts.push(effort.display_value());
    }
    for option in config.options() {
        let shown = option.is_category(&Category::Model)
            || option.is_category(&Category::Mode)
            || effort.is_some_and(|effort| effort.id == option.id);
        let value = option.display_value();
        if !shown && !value.eq_ignore_ascii_case("default") {
            parts.push(value);
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

impl ChatWidget {
    /// Show the agent in the footer now, before it has told us anything else about itself.
    pub(crate) fn show_agent_in_footer(&mut self) {
        let label = footer_label(self.bottom_pane.agent_display_name(), &self.agent_config);
        self.bottom_pane.set_model_label(label);
    }

    /// The agent's advertised configuration, in advertised order.
    pub(crate) fn agent_config(&self) -> &AgentConfigState {
        &self.agent_config
    }

    /// Adopt a new configuration snapshot: publish it to the status views and
    /// re-derive the mode cycle the footer and hotkey use.
    fn adopt_agent_config(&mut self, config: AgentConfigState) {
        self.agent_status
            .set(AgentStatus::from_config(&self.config.active_agent, &config));
        let mode = config.mode_config();
        self.bottom_pane
            .set_model_label(footer_label(self.bottom_pane.agent_display_name(), &config));
        self.agent_config = config;
        self.apply_acp_mode_config_snapshot(self.acp_mode_config_generation, mode);
    }

    /// Handle a `session/update` config announcement: report what changed (or
    /// announce the initial set), then adopt it.
    pub(crate) fn handle_acp_session_config_update(
        &mut self,
        config_options: &[nori_protocol::acp::v1::SessionConfigOption],
    ) {
        let next = AgentConfigState::from_options(config_options);

        // The first announcement is not printed: the footer shows the agent, model, effort and
        // mode, and only what changes after that is worth a line in the history.
        if !self.agent_config.is_empty() {
            let changes = next.changes_since(&self.agent_config);
            if !changes.is_empty() {
                self.add_to_history(
                    crate::nori::session_config_history::new_agent_options_history_cell(
                        self.bottom_pane.agent_display_name(),
                        &changes,
                    ),
                );
            }
        }

        self.adopt_agent_config(next);
        self.request_redraw();
    }

    /// Adopt a configuration the agent returned outside the update stream:
    /// an explicit `session/get_config` snapshot or the options echoed by a
    /// successful `set_session_config_option`.
    pub(crate) fn sync_acp_session_config_snapshot(
        &mut self,
        config_options: &[nori_protocol::acp::v1::SessionConfigOption],
    ) {
        self.adopt_agent_config(AgentConfigState::from_options(config_options));
    }

    pub(crate) fn handle_acp_session_config_snapshot(
        &mut self,
        generation: i64,
        config_options: &[nori_protocol::acp::v1::SessionConfigOption],
    ) {
        if generation != self.acp_mode_config_generation {
            return;
        }

        self.sync_acp_session_config_snapshot(config_options);
    }

    /// The status model for a card that keeps following later configuration
    /// changes: the welcome card and the post-compaction header, both of which
    /// are written before the agent has advertised anything.
    pub(crate) fn live_status_view_model(&self) -> StatusViewModel {
        self.status_view_model(self.agent_status.clone(), false)
    }

    /// Write the welcome card now, before any agent session exists.
    ///
    /// Lazy activation does not send `session/new` until the first prompt, so
    /// waiting for `SessionStarted` leaves the session with no card at all
    /// while the user is reading and typing. Everything on the compact card
    /// except the agent's configuration is known locally at startup, so the
    /// card goes out immediately with the provider name alone on the agent
    /// row; `on_session_started` then announces the model and options the
    /// agent resolved.
    ///
    /// No-op once the card has been written, so the agent-switch path (whose
    /// candidate widget is hidden until it publishes `SessionStarted`) still
    /// gets its card at session start.
    pub(crate) fn emit_welcome_card(&mut self) {
        if !self.show_welcome_banner {
            return;
        }
        self.show_welcome_banner = false;
        self.add_to_history(crate::history_cell::new_session_info(
            &self.config,
            self.config.active_agent.clone(),
            true,
            self.live_status_view_model(),
        ));
    }

    /// Take over a conversation whose welcome card is already in the
    /// scrollback, so this widget does not write a second one when its session
    /// starts.
    pub(crate) fn suppress_welcome_card(&mut self) {
        self.show_welcome_banner = false;
    }

    /// `with_instructions` also reads and tokenizes the instruction files, which the compact card
    /// never shows and which costs the first frame about 400 ms (the tokenizer loads).
    fn status_view_model(
        &self,
        agent: AgentStatusHandle,
        with_instructions: bool,
    ) -> StatusViewModel {
        let footer = self.bottom_pane.status_footer_values();
        let mut model = StatusViewModel::new(agent, self.config.cwd.clone());
        model.approval_mode_label =
            approval_mode_label(self.config.approval_policy, &self.config.sandbox_policy);
        model.skillset = SkillsetStatus {
            names: footer.active_skillsets,
            version: footer.nori_version,
            version_source: footer.nori_version_source,
        };
        if with_instructions {
            model.instruction_files = crate::nori::session_header::local_instruction_files(
                &self.config.active_agent,
                &self.config.cwd,
            );
        }
        model.prompt_summary = footer.prompt_summary;
        model.session_title = footer.session_title;
        model.conversation_id = self.conversation_id();
        model.forked_from = self.forked_from;
        model.cloud_session = self.cloud_session_identity();
        model.git = footer.git;
        model.context = footer.context;
        model.token_breakdown = footer.token_breakdown;
        model
    }

    /// Render the `/status` card over a detached snapshot of the agent status,
    /// because printed output must not change after the fact.
    pub(crate) fn add_status_output(&mut self) {
        let model = self.status_view_model(self.agent_status.snapshot(), true);
        self.add_to_history(crate::nori::session_header::new_nori_status_output(model));
    }
}
