//! Drawing the switcher: rail, query, list, divider, preview and key hints.

use nori_config::HotkeyBinding;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use super::preview;
use super::row::row_line;
use super::switcher::PREVIEW_PAD;
use super::switcher::Switcher;
use super::switcher::layout;

const EMPTY_HINT: &str = "no subagents yet; agents delegate with delegate_task";

impl Switcher {
    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer, toggle: &HotkeyBinding) {
        Clear.render(area, buf);
        let parts = layout(area);
        render_rail(parts.header, buf);
        self.render_query(parts.query, buf);
        self.render_list(parts.list, buf);
        if let Some(divider) = parts.divider {
            for y in divider.y..divider.bottom() {
                "│".dim().render(Rect::new(divider.x, y, 1, 1), buf);
            }
        }
        if let Some(pane) = parts.preview {
            preview::render(
                pane,
                buf,
                self.list.selected_entry(),
                self.body.as_ref(),
                PREVIEW_PAD,
                self.tick,
                self.animations,
            );
        }
        self.render_footer(parts.footer, buf, toggle);
    }

    fn render_query(&self, area: Rect, buf: &mut Buffer) {
        let query = self.list.query();
        let mut spans: Vec<Span<'static>> = vec!["› ".bold()];
        if query.is_empty() {
            spans.push("│".dim());
            spans.push(" type to filter subagents".dim());
        } else {
            spans.push(query.to_owned().into());
            spans.push("│".dim());
        }
        Line::from(spans).render(area, buf);
    }

    fn render_list(&mut self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let height = usize::from(area.height);
        self.list.scroll_to_selection(height);
        let selected = self.list.selected();
        let mut lines: Vec<Line<'static>> = self
            .list
            .rows()
            .iter()
            .skip(self.list.scroll_top)
            .take(height)
            .map(|row| {
                row_line(
                    row,
                    row.entry.target == selected,
                    usize::from(area.width),
                    self.tick,
                    self.animations,
                )
            })
            .collect();
        if self.list.rows().is_empty() {
            lines.push(Line::from("  no matches").dim().italic());
        } else if !self.list.has_subagents() && self.list.query().is_empty() {
            let room = usize::from(area.width).saturating_sub(2).max(1);
            lines.extend(
                textwrap::wrap(EMPTY_HINT, room)
                    .into_iter()
                    .map(|row| Line::from(format!("  {row}")).dim()),
            );
        }
        Paragraph::new(lines).render(area, buf);
    }

    fn render_footer(&self, area: Rect, buf: &mut Buffer, toggle: &HotkeyBinding) {
        if area.height == 0 {
            return;
        }
        let mut hints = String::from(" ↑↓ move · enter switch · esc close");
        if !toggle.is_none() {
            hints = format!("{hints} · {} close", toggle.as_str());
        }
        Line::from(hints).dim().render(area, buf);
        let count = match self.list.selected_index() {
            Some(index) => format!("{}/{} ", index + 1, self.list.rows().len()),
            None => "0/0 ".to_owned(),
        };
        let width = u16::try_from(UnicodeWidthStr::width(count.as_str())).unwrap_or(0);
        if width < area.width {
            count
                .dim()
                .render(Rect::new(area.right() - width, area.y, width, 1), buf);
        }
    }
}

/// The dotted rail the pagers draw across their top row, with the title over it.
pub(crate) fn render_rail(area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    Span::from("/ ".repeat(usize::from(area.width) / 2))
        .dim()
        .render(area, buf);
    "/ S U B A G E N T S".dim().render(area, buf);
}
