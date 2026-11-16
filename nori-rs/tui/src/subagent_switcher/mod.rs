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

pub(crate) use preview::header_lines;
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

use crate::pager_overlay::SubagentFocus;
use crate::tui;
use crate::tui::TuiEvent;

/// What the user asked for that the overlay cannot do itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    /// Leave for the normal chat view.
    Close,
    /// Open the focused view of a subagent.
    Focus(SubagentKey),
    /// Go back to the list.
    OpenSwitcher,
}

enum Mode {
    Switcher(Box<Switcher>),
    Focused(Box<SubagentFocus>),
}

pub(crate) struct SubagentOverlay {
    mode: Mode,
    /// The binding that opened the switcher; it closes it, and opens it from the focused view.
    toggle: HotkeyBinding,
    transition: Option<Transition>,
}

impl SubagentOverlay {
    /// The list of the main session and every subagent.
    pub(crate) fn switcher(roster: &Roster, toggle: HotkeyBinding) -> Self {
        Self {
            mode: Mode::Switcher(Box::new(Switcher::new(roster))),
            toggle,
            transition: None,
        }
    }

    /// The conversation of one subagent, if it is still known.
    pub(crate) fn focused(
        roster: &Roster,
        subagent: SubagentKey,
        toggle: HotkeyBinding,
        width: u16,
    ) -> Option<Self> {
        let focus = SubagentFocus::new(roster, Target::Subagent(subagent), width)?;
        Some(Self {
            mode: Mode::Focused(Box::new(focus)),
            toggle,
            transition: None,
        })
    }

    /// The transition the last event asked for, once.
    pub(crate) fn take_transition(&mut self) -> Option<Transition> {
        self.transition.take()
    }

    pub(crate) fn is_done(&self) -> bool {
        self.transition == Some(Transition::Close)
    }

    /// The conversation tail the overlay shows and does not have, for a screen of `area`.
    pub(crate) fn body_query(&self, area: Rect, now: Instant) -> Option<BodyQuery> {
        match &self.mode {
            Mode::Switcher(switcher) => switcher.body_query(area, now),
            Mode::Focused(focus) => focus.body_query(area, now),
        }
    }

    /// Take a fresh roster.
    pub(crate) fn update(&mut self, roster: &Roster, area: Rect) {
        match &mut self.mode {
            Mode::Switcher(switcher) => switcher.apply(roster),
            Mode::Focused(focus) => focus.apply(roster, None, area.width),
        }
    }

    /// Take the body that [`Self::body_query`] asked for.
    pub(crate) fn set_body(&mut self, roster: &Roster, body: Body, area: Rect) {
        match &mut self.mode {
            Mode::Switcher(switcher) => switcher.set_body(body),
            Mode::Focused(focus) => focus.apply(roster, Some(body), area.width),
        }
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        match &mut self.mode {
            Mode::Switcher(switcher) => switcher.render(area, buf, &self.toggle),
            Mode::Focused(focus) => focus.render(area, buf, &self.toggle),
        }
    }

    pub(crate) fn on_key(&mut self, key: KeyEvent) {
        let transition = match &mut self.mode {
            Mode::Switcher(switcher) => switcher.on_key(key, &self.toggle),
            Mode::Focused(focus) => focus.on_key(key, &self.toggle),
        };
        if transition.is_some() {
            self.transition = transition;
        }
    }

    pub(crate) fn handle_event(&mut self, tui: &mut tui::Tui, event: TuiEvent) -> Result<()> {
        match event {
            TuiEvent::Key(key) => {
                self.on_key(key);
                tui.frame_requester().schedule_frame();
            }
            TuiEvent::Draw => {
                tui.draw(u16::MAX, 0, |frame| {
                    self.render(frame.area(), frame.buffer);
                })?;
            }
            TuiEvent::Paste(_) | TuiEvent::Mouse(_) => {}
        }
        Ok(())
    }
}
