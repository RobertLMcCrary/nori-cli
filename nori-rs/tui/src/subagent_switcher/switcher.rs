//! The switcher itself: the query, the list, and the preview of the selected row.

use std::time::Instant;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use nori_config::HotkeyBinding;
use ratatui::layout::Position;
use ratatui::layout::Rect;

use super::Transition;
use super::list::ListState;
use super::roster::Body;
use super::roster::BodyQuery;
use super::roster::Roster;
use super::roster::Target;
use crate::nori::hotkey_match::matches_binding;

/// Terminals at least this wide get the preview pane beside the list.
pub(super) const PREVIEW_MIN_WIDTH: u16 = 100;
/// The share of the width the list keeps when the preview is shown, in percent.
const LIST_SHARE: u16 = 45;

/// Where each part of the switcher goes on a screen of a given size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Layout {
    pub(super) header: Rect,
    pub(super) query: Rect,
    pub(super) list: Rect,
    /// The column between list and preview.
    pub(super) divider: Option<Rect>,
    pub(super) preview: Option<Rect>,
    pub(super) footer: Rect,
}

pub(super) fn layout(area: Rect) -> Layout {
    let header = Rect::new(area.x, area.y, area.width, area.height.min(1));
    let query = Rect::new(
        area.x,
        area.y + header.height,
        area.width,
        area.height.saturating_sub(1).min(1),
    );
    let footer_height = u16::from(area.height > 3);
    let top = header.height + query.height;
    let body_height = area.height.saturating_sub(top + footer_height);
    let body_y = area.y + top;
    let footer = Rect::new(area.x, body_y + body_height, area.width, footer_height);
    if area.width < PREVIEW_MIN_WIDTH {
        return Layout {
            header,
            query,
            list: Rect::new(area.x, body_y, area.width, body_height),
            divider: None,
            preview: None,
            footer,
        };
    }
    let list_width = area.width * LIST_SHARE / 100;
    Layout {
        header,
        query,
        list: Rect::new(area.x, body_y, list_width, body_height),
        divider: Some(Rect::new(area.x + list_width, body_y, 1, body_height)),
        preview: Some(Rect::new(
            area.x + list_width + 1,
            body_y,
            area.width - list_width - 1,
            body_height,
        )),
        footer,
    }
}

/// A column of left padding inside the preview, so text does not touch the divider.
pub(super) const PREVIEW_PAD: u16 = 1;

pub(crate) struct Switcher {
    pub(super) list: ListState,
    /// The tail of the selected row's conversation, once fetched.
    pub(super) body: Option<Body>,
    pub(super) animations: bool,
    pub(super) tick: i64,
    pointer: Option<(Target, u16, u16, bool)>,
}

impl Switcher {
    pub(crate) fn new(roster: &Roster) -> Self {
        Self {
            list: ListState::new(roster),
            body: None,
            animations: roster.animations,
            tick: roster.tick,
            pointer: None,
        }
    }

    pub(crate) fn apply(&mut self, roster: &Roster) {
        self.animations = roster.animations;
        self.tick = roster.tick;
        self.list.apply(roster);
    }

    /// The conversation tail the preview needs and does not have yet.
    pub(crate) fn body_query(&self, area: Rect, now: Instant) -> Option<BodyQuery> {
        let preview = layout(area).preview?;
        let entry = self.list.selected_entry()?;
        let query = BodyQuery {
            target: entry.target,
            width: preview.width.saturating_sub(PREVIEW_PAD),
            rows: usize::from(preview.height),
        };
        match &self.body {
            Some(body) if body.is_current(entry, query, now) => None,
            Some(_) | None => Some(query),
        }
    }

    pub(crate) fn set_body(&mut self, body: Body) {
        self.body = Some(body);
    }

    /// Act on a key. `toggle` is the binding that opened the switcher, which also closes it.
    pub(crate) fn on_key(&mut self, key: KeyEvent, toggle: &HotkeyBinding) -> Option<Transition> {
        self.pointer = None;
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return None;
        }
        if matches_binding(toggle, &key) {
            return Some(Transition::Close);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let plain = !ctrl && !key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => return Some(Transition::Close),
            KeyCode::Char('c') if ctrl => return Some(Transition::Close),
            KeyCode::Enter => {
                return match self.list.selected_entry().map(|entry| entry.target) {
                    Some(Target::Main) => Some(Transition::FocusMain),
                    Some(Target::Subagent(subagent)) => Some(Transition::Focus(subagent)),
                    None => None,
                };
            }
            KeyCode::Up => self.list.step(-1),
            KeyCode::Down => self.list.step(1),
            KeyCode::Char('p') if ctrl => self.list.step(-1),
            KeyCode::Char('n') if ctrl => self.list.step(1),
            KeyCode::PageUp => self.list.jump(-page(self.list.page)),
            KeyCode::PageDown => self.list.jump(page(self.list.page)),
            KeyCode::Home => self.list.first(),
            KeyCode::End => self.list.last(),
            KeyCode::Backspace => self.list.pop_char(),
            KeyCode::Char('u') if ctrl => self.list.clear_query(),
            KeyCode::Char('w') if ctrl => self.list.delete_word(),
            KeyCode::Char(ch) if plain => self.list.push_char(ch),
            _ => {}
        }
        None
    }

    pub(crate) fn on_mouse(&mut self, mouse: MouseEvent, area: Rect) -> Option<Transition> {
        let parts = layout(area);
        let inside = parts.list.contains(Position::new(mouse.column, mouse.row));
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if inside => {
                self.pointer = None;
                self.list.step(if mouse.kind == MouseEventKind::ScrollUp {
                    -1
                } else {
                    1
                });
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.pointer = inside
                    .then(|| usize::from(mouse.row - parts.list.y) + self.list.scroll_top)
                    .and_then(|row| self.list.rows().get(row))
                    .map(|row| (row.entry.target, mouse.column, mouse.row, false));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((_, column, last_row, dragged)) = &mut self.pointer {
                    let delta =
                        isize::try_from(i32::from(*last_row) - i32::from(mouse.row)).unwrap_or(0);
                    *dragged |= mouse.column != *column || delta != 0;
                    *last_row = mouse.row;
                    if delta != 0 {
                        self.list.jump(delta);
                        self.list
                            .scroll_to_selection(usize::from(parts.list.height));
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some((target, _, _, false)) = self.pointer.take() else {
                    return None;
                };
                if !inside {
                    return None;
                }
                let row = usize::from(mouse.row - parts.list.y) + self.list.scroll_top;
                if self.list.rows().get(row).map(|row| row.entry.target) != Some(target) {
                    return None;
                }
                self.list.select_index_at(row);
                return Some(match target {
                    Target::Main => Transition::FocusMain,
                    Target::Subagent(subagent) => Transition::Focus(subagent),
                });
            }
            MouseEventKind::Down(_) | MouseEventKind::Up(_) => self.pointer = None,
            _ => {}
        }
        None
    }
}

fn page(rows: usize) -> isize {
    isize::try_from(rows).unwrap_or(isize::MAX).max(1)
}
