//! The preview pane: who the selected row is, then the tail of its conversation.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use super::roster::Body;
use super::roster::Entry;
use super::roster::Status;
use super::row::highlighted;
use super::row::status_glyph;
use super::row::timing_parts;

/// The task, when the title is cut short, shows over at most this many rows.
const TASK_MAX_ROWS: usize = 4;

/// The rows that say who `entry` is: the task, what it is, how it is doing.
pub(crate) fn header_lines(
    entry: &Entry,
    width: u16,
    tick: i64,
    animations: bool,
) -> Vec<Line<'static>> {
    let width = usize::from(width);
    let title_room = width.saturating_sub(2);
    let (title, title_width) = highlighted(&entry.title, &[], Style::default().bold(), title_room);
    let mut first = vec![status_glyph(entry.status, tick, animations), " ".into()];
    first.extend(title);
    let mut lines = vec![Line::from(first)];

    let mut facts: Vec<String> = Vec::new();
    facts.extend(entry.agent.clone());
    facts.extend(entry.model.clone());
    facts.extend(
        entry
            .parent
            .as_ref()
            .map(|parent| format!("under {parent}")),
    );
    facts.push(match &entry.detail {
        Some(detail) if entry.status == Status::Failed => format!("failed: {detail}"),
        Some(_) | None => entry.status.word().to_owned(),
    });
    facts.extend(timing_parts(entry));
    lines.push(Line::from(facts.join(" · ")).dim());

    if title_width < UnicodeWidthStr::width(entry.title.as_str()) {
        // The row had to cut the task short; the pane has room for all of it.
        let wrapped = textwrap::wrap(&entry.title, width.max(1));
        let more = wrapped.len() > TASK_MAX_ROWS;
        lines.extend(
            wrapped
                .iter()
                .take(TASK_MAX_ROWS)
                .map(|row| Line::from(row.to_string()).dim()),
        );
        if more {
            lines.push(Line::from("…").dim());
        }
    }
    lines
}

/// Draw the preview of `entry` in `area`, whose first column is padding.
pub(super) fn render(
    area: Rect,
    buf: &mut Buffer,
    entry: Option<&Entry>,
    body: Option<&Body>,
    pad: u16,
    tick: i64,
    animations: bool,
) {
    let area = Rect::new(
        area.x + pad.min(area.width),
        area.y,
        area.width.saturating_sub(pad),
        area.height,
    );
    let Some(entry) = entry else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut lines = header_lines(entry, area.width, tick, animations);
    lines.push(Line::from("─".repeat(usize::from(area.width))).dim());
    let used = lines.len();
    let room = usize::from(area.height).saturating_sub(used);
    let tail = body
        .filter(|body| body.query.target == entry.target)
        .map(|body| body.lines.as_slice())
        .unwrap_or_default();
    if tail.is_empty() {
        lines.push(Line::from(empty_text(entry)).dim().italic());
    } else {
        let skip = tail.len().saturating_sub(room);
        lines.extend(tail[skip..].iter().cloned());
    }
    Paragraph::new(Text::from(lines)).render(area, buf);
}

fn empty_text(entry: &Entry) -> &'static str {
    match entry.status {
        Status::Running => "starting…",
        Status::Idle | Status::Completed | Status::Failed | Status::Cancelled => "nothing yet",
    }
}
