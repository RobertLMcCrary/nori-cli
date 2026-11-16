//! One row of the list: pointer, status glyph, task, and the dim details after it.

use std::time::Duration;

use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use super::list::Row;
use super::roster::Entry;
use super::roster::Status;
use crate::chatwidget::subagents::SPINNER_FRAMES;
use crate::chatwidget::subagents::format_elapsed;
use crate::chatwidget::subagents::tool_count_text;

/// The longest the name of a parent is allowed to be in `parent › child`.
const PARENT_MAX_WIDTH: usize = 16;
/// Columns taken before the task: pointer and glyph, each followed by a space.
const ROW_PREFIX_WIDTH: usize = 4;

/// The glyph for a status, consistent with the live block and the summary cells: the same spinner
/// while running, a green check, a red cross, a dim slashed circle.
pub(crate) fn status_glyph(status: Status, tick: i64, animations: bool) -> Span<'static> {
    match status {
        Status::Running if animations => {
            let frame = usize::try_from(tick.rem_euclid(10)).unwrap_or(0);
            SPINNER_FRAMES[frame].cyan()
        }
        Status::Running | Status::Idle => "•".dim(),
        Status::Completed => "✓".green(),
        Status::Failed => "✗".red(),
        Status::Cancelled => "⊘".dim(),
    }
}

/// `text` in runs of one style, with the characters at `indices` set bold, cut to `max_width`
/// columns with an ellipsis. Returns the spans and the columns they take.
pub(crate) fn highlighted(
    text: &str,
    indices: &[usize],
    base: Style,
    max_width: usize,
) -> (Vec<Span<'static>>, usize) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_matched = false;
    let mut used = 0;
    let mut matches = indices.iter().copied().peekable();
    let total = UnicodeWidthStr::width(text);
    // An ellipsis takes the place of the last column when the text does not fit.
    let limit = if total > max_width {
        max_width.saturating_sub(1)
    } else {
        max_width
    };
    let mut cut = false;
    for (index, ch) in text.chars().enumerate() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > limit {
            cut = true;
            break;
        }
        used += width;
        let matched = matches.next_if_eq(&index).is_some();
        if matched != run_matched && !run.is_empty() {
            spans.push(styled(std::mem::take(&mut run), run_matched, base));
        }
        run_matched = matched;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(styled(run, run_matched, base));
    }
    if cut {
        spans.push(Span::styled("…", base));
        used += 1;
    }
    (spans, used)
}

fn styled(text: String, matched: bool, base: Style) -> Span<'static> {
    if matched {
        Span::styled(text, Style::default().bold())
    } else {
        Span::styled(text, base)
    }
}

/// How long ago, in the coarsest unit that is not zero: `just now`, `12s ago`, `2m ago`, `3h ago`.
pub(crate) fn ago(elapsed: Duration) -> String {
    match elapsed.as_secs() {
        0..=4 => "just now".to_owned(),
        seconds @ 5..=59 => format!("{seconds}s ago"),
        seconds @ 60..=3599 => format!("{}m ago", seconds / 60),
        seconds => format!("{}h ago", seconds / 3600),
    }
}

/// The line for one row at `width` columns.
pub(super) fn row_line(
    row: &Row,
    selected: bool,
    width: usize,
    tick: i64,
    animations: bool,
) -> Line<'static> {
    let entry = &row.entry;
    let mut spans: Vec<Span<'static>> = vec![
        if selected {
            "› ".green()
        } else {
            "  ".into()
        },
        status_glyph(entry.status, tick, animations),
        " ".into(),
    ];
    let mut used = ROW_PREFIX_WIDTH;
    if let Some(parent) = &entry.parent {
        let (name, _) = highlighted(parent, &[], Style::default().dim(), PARENT_MAX_WIDTH);
        let name_width: usize = name.iter().map(Span::width).sum();
        spans.extend(name);
        spans.push(" › ".dim());
        used += name_width + 3;
    }
    let (title, title_width) = highlighted(
        &entry.title,
        &row.highlights.title,
        Style::default(),
        width.saturating_sub(used),
    );
    spans.extend(title);
    used += title_width;
    if title_width < UnicodeWidthStr::width(entry.title.as_str()) {
        return Line::from(spans);
    }
    append_details(&mut spans, row, used, width);
    Line::from(spans)
}

/// The agent, model, tool count and time, as many as fit after the task. When they do not all
/// fit, the time and the tool count win over the agent and the model.
fn append_details(spans: &mut Vec<Span<'static>>, row: &Row, used: usize, width: usize) {
    let entry = &row.entry;
    let dim = Style::default().dim();
    // In display order, each with its priority: lower is kept first.
    let mut parts: Vec<(i32, Vec<Span<'static>>)> = Vec::new();
    if let Some(agent) = &entry.agent {
        parts.push((
            2,
            highlighted(agent, &row.highlights.agent, dim, usize::MAX).0,
        ));
    }
    if let Some(model) = &entry.model {
        parts.push((
            3,
            highlighted(model, &row.highlights.model, dim, usize::MAX).0,
        ));
    }
    let timing = timing_parts(entry);
    let last = timing.len().saturating_sub(1);
    parts.extend(timing.into_iter().enumerate().map(|(index, text)| {
        // How long it ran comes first of all; how many tools and how long ago come next.
        (i32::from(index != last), vec![text.dim()])
    }));
    let widths: Vec<usize> = parts
        .iter()
        .map(|(_, part)| part.iter().map(Span::width).sum())
        .collect();
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by_key(|index| parts[*index].0);
    let mut kept = vec![false; parts.len()];
    // Two columns of gap before the first part, three between parts.
    let mut taken = used + 2;
    let mut count = 0;
    for index in order {
        let separator = if count == 0 { 0 } else { 3 };
        if taken + separator + widths[index] <= width {
            kept[index] = true;
            taken += separator + widths[index];
            count += 1;
        }
    }
    let mut first = true;
    for (index, (_, part)) in parts.into_iter().enumerate() {
        if !kept[index] {
            continue;
        }
        spans.push(if first { "  ".into() } else { " · ".dim() });
        spans.extend(part);
        first = false;
    }
}

/// Tool count, then how long it ran and, once ended, how long ago.
pub(super) fn timing_parts(entry: &Entry) -> Vec<String> {
    let mut parts = Vec::new();
    if entry.tool_count > 0 {
        parts.push(tool_count_text(entry.tool_count));
    }
    match (entry.status, entry.ended_ago) {
        (Status::Idle, _) => {}
        (Status::Running, _) | (Status::Completed | Status::Failed | Status::Cancelled, None) => {
            parts.push(format_elapsed(entry.elapsed));
        }
        (Status::Completed | Status::Failed | Status::Cancelled, Some(ended)) => {
            parts.push(format_elapsed(entry.elapsed));
            parts.push(ago(ended));
        }
    }
    parts
}
