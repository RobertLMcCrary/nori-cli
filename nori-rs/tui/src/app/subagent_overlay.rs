//! The subagent switcher in the app: opening it, feeding it what it needs to show, and acting on
//! where the user takes it.

use std::time::Instant;

use nori_config::HotkeyAction;
use ratatui::layout::Rect;

use super::*;
use crate::subagent_switcher::Body;
use crate::subagent_switcher::BodyQuery;
use crate::subagent_switcher::Roster;
use crate::subagent_switcher::SubagentOverlay;
use crate::subagent_switcher::Tail;
use crate::subagent_switcher::Target;
use crate::subagent_switcher::Transition;
use crate::subagent_switcher::cell_lines;

impl App {
    /// Open the list of the main session and every subagent.
    pub(super) fn open_subagent_switcher(&mut self, tui: &mut tui::Tui) {
        let roster = self.subagent_roster();
        let overlay = SubagentOverlay::switcher(&roster, self.switcher_binding());
        self.show_subagent_overlay(tui, overlay);
    }

    fn switcher_binding(&self) -> nori_config::HotkeyBinding {
        self.hotkey_config
            .binding_for(HotkeyAction::SwitchSubagent)
            .clone()
    }

    fn show_subagent_overlay(&mut self, tui: &mut tui::Tui, overlay: SubagentOverlay) {
        if self.overlay.is_none() {
            let _ = tui.enter_alt_screen();
        }
        self.overlay = Some(Overlay::Subagents(Box::new(overlay)));
        tui.frame_requester().schedule_frame();
    }

    fn subagent_roster(&self) -> Roster {
        let mut roster = self.chat_widget.subagent_roster(Instant::now());
        // The root conversation's revision is how many cells it has: its preview changes with them.
        roster.main.revision = i64::try_from(self.transcript_cells.len()).unwrap_or(i64::MAX);
        roster
    }

    /// The tail of the root conversation: its last cells, as the transcript overlay shows them.
    fn main_body(&self, query: BodyQuery, revision: i64, now: Instant) -> Body {
        let mut tail = Tail::new(query.rows);
        for cell in self.transcript_cells.iter().rev() {
            if tail.is_full() {
                break;
            }
            tail.push_front(cell_lines(cell.as_ref(), query.width));
        }
        Body {
            query,
            revision,
            lines: tail.finish(),
            built_at: now,
        }
    }

    /// Bring the open switcher up to date before a frame: a fresh roster, and the conversation
    /// tail it lacks, if any. A frame in which nothing changed costs only the roster.
    fn feed_subagent_overlay(&mut self, tui: &mut tui::Tui) -> color_eyre::eyre::Result<()> {
        let size = tui.terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let now = Instant::now();
        let roster = self.subagent_roster();
        let Some(Overlay::Subagents(overlay)) = &mut self.overlay else {
            return Ok(());
        };
        overlay.update(&roster, area);
        let Some(query) = overlay.body_query(area, now) else {
            return Ok(());
        };
        let body = match query.target {
            Target::Main => Some(self.main_body(query, roster.main.revision, now)),
            Target::Subagent(_) => self.chat_widget.subagent_body(query, now),
        };
        if let Some(body) = body
            && let Some(Overlay::Subagents(overlay)) = &mut self.overlay
        {
            overlay.set_body(&roster, body, area);
        }
        Ok(())
    }

    /// Route an event to the open switcher or focused view, then carry out where it asked to go.
    pub(super) fn handle_subagent_overlay_event(
        &mut self,
        tui: &mut tui::Tui,
        event: TuiEvent,
    ) -> Result<()> {
        if matches!(event, TuiEvent::Draw) {
            self.chat_widget.subagent_overlay_tick();
            self.feed_subagent_overlay(tui)?;
        }
        let Some(Overlay::Subagents(overlay)) = &mut self.overlay else {
            return Ok(());
        };
        overlay.handle_event(tui, event)?;
        let Some(transition) = overlay.take_transition() else {
            return Ok(());
        };
        match transition {
            Transition::Close => {
                self.close_transcript_overlay(tui);
                tui.frame_requester().schedule_frame();
            }
            Transition::OpenSwitcher => self.open_subagent_switcher(tui),
            Transition::Focus(subagent) => {
                let roster = self.subagent_roster();
                let width = tui.terminal.size()?.width;
                if let Some(focused) =
                    SubagentOverlay::focused(&roster, subagent, self.switcher_binding(), width)
                {
                    self.show_subagent_overlay(tui, focused);
                }
            }
        }
        Ok(())
    }
}
