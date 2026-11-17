//! The subagent switcher: Ctrl+Space opens a fuzzy finder over the main session and every
//! subagent, with a live preview of the one under the cursor. Enter on a subagent opens a focused
//! view of its conversation that follows its output.
//!
//! The overlay takes snapshots ([`Roster`], [`Body`]) and never reaches into the chat widget. The
//! app is the go-between: before each frame it asks the overlay what it lacks
//! ([`SubagentOverlay::body_query`]), fetches that, and hands it back with a fresh roster
//! ([`SubagentOverlay::update`]). A frame in which nothing changed asks for nothing.

mod list;
mod preview;
mod render;
mod roster;
mod row;
mod switcher;
mod tail;

#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
mod tests;

use std::io::Result;
use std::time::Instant;

use crossterm::event::KeyEvent;
use nori_config::HotkeyBinding;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

pub(crate) use roster::Body;
pub(crate) use roster::BodyQuery;
pub(crate) use roster::Entry;
pub(crate) use roster::Roster;
pub(crate) use roster::Status;
pub(crate) use roster::SubagentKey;
pub(crate) use roster::Target;
use switcher::Switcher;
pub(crate) use tail::Tail;
pub(crate) use tail::cell_lines;

use crate::tui;
use crate::tui::TuiEvent;

/// What the user asked for that the overlay cannot do itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    /// Leave for the normal chat view.
    Close,
    /// Open the main conversation.
    FocusMain,
    /// Open the focused view of a subagent.
    Focus(SubagentKey),
}

pub(crate) struct SubagentOverlay {
    switcher: Switcher,
    toggle: HotkeyBinding,
    transition: Option<Transition>,
    area: Rect,
}

impl SubagentOverlay {
    pub(crate) fn switcher(roster: &Roster, toggle: HotkeyBinding) -> Self {
        Self {
            switcher: Switcher::new(roster),
            toggle,
            transition: None,
            area: Rect::default(),
        }
    }

    pub(crate) fn take_transition(&mut self) -> Option<Transition> {
        self.transition.take()
    }

    pub(crate) fn is_done(&self) -> bool {
        self.transition == Some(Transition::Close)
    }

    pub(crate) fn body_query(&self, area: Rect, now: Instant) -> Option<BodyQuery> {
        self.switcher.body_query(area, now)
    }

    pub(crate) fn update(&mut self, roster: &Roster, _area: Rect) {
        self.switcher.apply(roster);
    }

    pub(crate) fn set_body(&mut self, _roster: &Roster, body: Body, _area: Rect) {
        self.switcher.set_body(body);
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.switcher.render(area, buf, &self.toggle);
    }

    pub(crate) fn on_key(&mut self, key: KeyEvent) {
        if let Some(transition) = self.switcher.on_key(key, &self.toggle) {
            self.transition = Some(transition);
        }
    }

    pub(crate) fn handle_event(&mut self, tui: &mut tui::Tui, event: TuiEvent) -> Result<()> {
        match event {
            TuiEvent::Key(key) => {
                self.on_key(key);
                tui.frame_requester().schedule_frame();
            }
            TuiEvent::Paste(text) => {
                for character in text.chars().filter(|character| !character.is_control()) {
                    self.on_key(KeyEvent::new(
                        crossterm::event::KeyCode::Char(character),
                        crossterm::event::KeyModifiers::NONE,
                    ));
                }
                tui.frame_requester().schedule_frame();
            }
            TuiEvent::Draw => {
                tui.draw_overlay(|frame| {
                    self.area = frame.area();
                    self.render(frame.area(), frame.buffer);
                })?;
            }
            TuiEvent::Mouse(mouse) => {
                if mouse.kind == crossterm::event::MouseEventKind::Moved {
                    return Ok(());
                }
                self.transition = self.switcher.on_mouse(mouse, self.area);
                tui.frame_requester().schedule_frame();
            }
        }
        Ok(())
    }
}
