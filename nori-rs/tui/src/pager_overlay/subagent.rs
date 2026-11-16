//! The focused view of one subagent: its whole conversation in the pager, following new output.
//!
//! The pager does the scrolling and the layout of rows; this adds what the subagent switcher
//! needs around it: a header that names the subagent, a body that is replaced as the subagent
//! works, and the keys that go back (Esc) or to the list (the switcher binding).

use std::time::Instant;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use nori_config::HotkeyBinding;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use super::KEY_DOWN;
use super::KEY_END;
use super::KEY_ESC;
use super::KEY_HOME;
use super::KEY_PAGE_DOWN;
use super::KEY_PAGE_UP;
use super::KEY_SPACE;
use super::KEY_UP;
use super::PAGER_KEY_HINTS;
use super::PagerView;
use super::render_key_hints;
use crate::nori::hotkey_match::matches_binding;
use crate::render::renderable::Renderable;
use crate::subagent_switcher::Body;
use crate::subagent_switcher::BodyQuery;
use crate::subagent_switcher::Entry;
use crate::subagent_switcher::Roster;
use crate::subagent_switcher::Target;
use crate::subagent_switcher::Transition;
use crate::subagent_switcher::header_lines;

/// The longest the task in the header rail is.
const TITLE_MAX_CHARS: usize = 60;
/// Rows per item of the pager, so a frame lays out and draws only the items in view.
const CHUNK_ROWS: usize = 64;

/// The rows that say who this is, over a rule that spans whatever width the pager has.
struct Header(Paragraph<'static>);

impl Renderable for Header {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let rows = self.0.line_count(area.width).min(usize::from(area.height));
        let rows = u16::try_from(rows).unwrap_or(area.height);
        Renderable::render(&self.0, Rect::new(area.x, area.y, area.width, rows), buf);
        if rows < area.height {
            Line::from("─".repeat(usize::from(area.width)))
                .dim()
                .render(Rect::new(area.x, area.y + rows, area.width, 1), buf);
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.0
            .line_count(width)
            .try_into()
            .unwrap_or(u16::MAX)
            .saturating_add(1)
    }
}

pub(crate) struct SubagentFocus {
    target: Target,
    entry: Entry,
    /// What the body was built from, without its lines (the view owns those).
    body: Option<Body>,
    view: PagerView,
    /// How many rows the header took last time, to see when the view must be laid out again.
    header_rows: usize,
    tick: i64,
    animations: bool,
}

impl SubagentFocus {
    pub(crate) fn new(roster: &Roster, target: Target, width: u16) -> Option<Self> {
        let entry = roster.entry(target)?.clone();
        let mut focus = Self {
            target,
            view: PagerView::new(Vec::new(), String::new(), usize::MAX),
            entry,
            body: None,
            header_rows: 0,
            tick: roster.tick,
            animations: roster.animations,
        };
        focus.apply(roster, None, width);
        Some(focus)
    }

    /// The whole conversation, if the view does not have its current state.
    pub(crate) fn body_query(&self, area: Rect, now: Instant) -> Option<BodyQuery> {
        let query = BodyQuery {
            target: self.target,
            width: area.width,
            rows: usize::MAX,
        };
        match &self.body {
            Some(body) if body.is_current(&self.entry, query, now) => None,
            Some(_) | None => Some(query),
        }
    }

    /// Take the subagent's current state, and its new body if one was asked for. A subagent the
    /// tree no longer knows keeps showing what it last was.
    pub(crate) fn apply(&mut self, roster: &Roster, body: Option<Body>, width: u16) {
        self.target = roster.resolve(self.target);
        if let Some(entry) = roster.entry(self.target) {
            self.entry = entry.clone();
        }
        self.tick = roster.tick;
        self.animations = roster.animations;
        let follow = self.view.is_scrolled_to_bottom();
        let header = header_lines(&self.entry, width, self.tick, self.animations);
        let rows = header.len() + 1;
        let header: Box<dyn Renderable> = Box::new(Header(Paragraph::new(Text::from(header))));
        match body {
            Some(mut body) => {
                let mut lines = std::mem::take(&mut body.lines).into_iter().peekable();
                self.body = Some(body);
                let mut items = vec![header];
                while lines.peek().is_some() {
                    let chunk: Vec<Line<'static>> = lines.by_ref().take(CHUNK_ROWS).collect();
                    items.push(Box::new(Paragraph::new(Text::from(chunk))));
                }
                self.view.replace_from(0, items);
            }
            None if rows == self.header_rows && !self.view.renderables.is_empty() => {
                self.view.renderables[0] = header;
            }
            None => {
                // The header grew or shrank: lay it out again, keeping the body after it.
                let body_items = self
                    .view
                    .renderables
                    .split_off(1.min(self.view.renderables.len()));
                self.view.replace_from(0, vec![header]);
                for item in body_items {
                    self.view.push_renderable(item);
                }
            }
        }
        self.header_rows = rows;
        let mut title: String = self.entry.title.chars().take(TITLE_MAX_CHARS).collect();
        if self.entry.title.chars().count() > TITLE_MAX_CHARS {
            title.push('…');
        }
        self.view.title = format!("{title} · {}", self.entry.status.word());
        if follow {
            self.view.scroll_offset = usize::MAX;
        }
    }

    pub(crate) fn on_key(&mut self, key: KeyEvent, toggle: &HotkeyBinding) -> Option<Transition> {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return None;
        }
        if matches_binding(toggle, &key) {
            return Some(Transition::OpenSwitcher);
        }
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if KEY_ESC.is_press(key) || ctrl_c {
            return Some(Transition::Close);
        }
        let page = self.view.last_content_height.unwrap_or(10).max(1);
        let view = &mut self.view;
        match key {
            e if KEY_UP.is_press(e) => view.scroll_offset = view.scroll_offset.saturating_sub(1),
            e if KEY_DOWN.is_press(e) => view.scroll_offset = view.scroll_offset.saturating_add(1),
            e if KEY_PAGE_UP.is_press(e) => {
                view.scroll_offset = view.scroll_offset.saturating_sub(page);
            }
            e if KEY_PAGE_DOWN.is_press(e) || KEY_SPACE.is_press(e) => {
                view.scroll_offset = view.scroll_offset.saturating_add(page);
            }
            e if KEY_HOME.is_press(e) => view.scroll_offset = 0,
            e if KEY_END.is_press(e) => view.scroll_offset = usize::MAX,
            _ => {}
        }
        None
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer, toggle: &HotkeyBinding) {
        let footer_rows = area.height.min(2);
        let top = Rect::new(area.x, area.y, area.width, area.height - footer_rows);
        self.view.render(top, buf);
        if footer_rows == 0 {
            return;
        }
        let mut hints = String::from(" esc back");
        if !toggle.is_none() {
            hints = format!("{hints} · {} switch", toggle.as_str());
        }
        Line::from(hints)
            .dim()
            .render(Rect::new(area.x, top.bottom(), area.width, 1), buf);
        if footer_rows == 2 {
            render_key_hints(
                Rect::new(area.x, top.bottom() + 1, area.width, 1),
                buf,
                PAGER_KEY_HINTS,
            );
        }
    }
}
