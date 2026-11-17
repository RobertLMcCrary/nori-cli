//! What the terminal wrote above its viewport, so that a repaint can write exactly that again.

use std::collections::VecDeque;

use ratatui::text::Line;
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::render::line_utils::line_to_static;

/// The newest history lines a terminal wrote above its viewport, oldest first, each with the
/// rows it takes. They end where the viewport starts: whatever moves the viewport without moving
/// them (clearing the screen, writing over them) must [`Self::clear`] the record.
///
/// Only lines that can still be on screen are kept: no more rows than the screen has, plus the
/// line cut by its top edge. So keeping the record costs a screenful, however much is written.
#[derive(Debug, Default, Clone, Eq, PartialEq, Hash)]
pub(crate) struct WrittenHistory {
    lines: VecDeque<(Line<'static>, usize)>,
    rows: usize,
    /// The width the lines were wrapped for. What the terminal did with them at another width is
    /// unknown, so they are not Nori's to rewrite there.
    width: u16,
}

impl WrittenHistory {
    /// Note that `lines`, wrapped for `width`, were written below what was written before, on a
    /// screen `screen_rows` tall.
    pub(crate) fn record(&mut self, lines: &[Line<'_>], width: u16, screen_rows: usize) {
        if width != self.width {
            self.clear();
            self.width = width;
        }
        let mut start = lines.len();
        let mut rows = 0;
        while start > 0 && rows < screen_rows {
            start -= 1;
            rows += rows_at(&lines[start], width);
        }
        for line in &lines[start..] {
            let rows = rows_at(line, width);
            self.rows += rows;
            self.lines.push_back((line_to_static(line), rows));
        }
        while let Some((_, oldest)) = self.lines.front()
            && self.rows - oldest >= screen_rows
        {
            self.rows -= oldest;
            self.lines.pop_front();
        }
    }

    /// Forget what was written, e.g. because the rows above the viewport were cleared.
    pub(crate) fn clear(&mut self) {
        self.lines.clear();
        self.rows = 0;
    }

    /// Rows of it on screen directly above a viewport `width` wide whose top is at `top`.
    pub(crate) fn owned_rows(&self, top: u16, width: u16) -> usize {
        if width == self.width {
            self.rows.min(usize::from(top))
        } else {
            0
        }
    }

    /// The newest `rows` rows, oldest first, as lines to write from the first of those rows. A
    /// line cut by that edge keeps only its rows below it.
    pub(crate) fn newest_rows(&self, rows: usize) -> Vec<Line<'static>> {
        let mut newest_first = Vec::new();
        let mut collected = 0;
        for (line, line_rows) in self.lines.iter().rev() {
            if collected >= rows {
                break;
            }
            let hidden_rows = (collected + line_rows).saturating_sub(rows);
            collected += line_rows;
            if hidden_rows == 0 {
                newest_first.push(line.clone());
                continue;
            }
            // Drop the columns of the rows that are above the edge.
            let width = usize::from(self.width.max(1));
            let mut row = 0;
            let mut column = 0;
            let mut visible = Line {
                spans: Vec::new(),
                ..line.clone()
            };
            for span in &line.spans {
                let mut text = String::new();
                for grapheme in span.content.graphemes(true) {
                    let cells = grapheme.width().min(width);
                    if cells > 0 && column + cells > width {
                        row += 1;
                        column = 0;
                    }
                    if row >= hidden_rows {
                        text.push_str(grapheme);
                    }
                    column += cells;
                }
                if !text.is_empty() {
                    visible.spans.push(Span::styled(text, span.style));
                }
            }
            newest_first.push(visible);
        }
        newest_first.reverse();
        newest_first
    }
}

/// Terminal rows `line` takes at `width`.
fn rows_at(line: &Line<'_>, width: u16) -> usize {
    crate::native_history::physical_rows(line, usize::from(width))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn lines(texts: &[&str]) -> Vec<Line<'static>> {
        texts
            .iter()
            .map(|text| Line::from(text.to_string()))
            .collect()
    }

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(Line::to_string).collect()
    }

    #[test]
    fn keeps_only_what_can_still_be_on_screen() {
        let mut history = WrittenHistory::default();
        history.record(&lines(&["one", "two", "three"]), 10, 4);
        history.record(&lines(&["four", "five", "six"]), 10, 4);

        assert_eq!(history.owned_rows(20, 10), 4);
        assert_eq!(
            text(&history.newest_rows(4)),
            vec!["three", "four", "five", "six"]
        );
        assert_eq!(text(&history.newest_rows(2)), vec!["five", "six"]);
    }

    #[test]
    fn owns_no_more_rows_than_are_above_the_viewport() {
        let mut history = WrittenHistory::default();
        history.record(&lines(&["one", "two", "three"]), 10, 12);

        assert_eq!(history.owned_rows(2, 10), 2);
        assert_eq!(history.owned_rows(9, 10), 3);
    }

    #[test]
    fn rows_written_at_another_width_are_not_owned() {
        let mut history = WrittenHistory::default();
        history.record(&lines(&["one", "two"]), 10, 12);

        assert_eq!(history.owned_rows(9, 12), 0);

        history.record(&lines(&["three"]), 12, 12);
        assert_eq!(text(&history.newest_rows(5)), vec!["three"]);
    }

    #[test]
    fn a_line_cut_by_the_edge_keeps_its_rows_below_it() {
        let mut history = WrittenHistory::default();
        // Three rows at width 4: "aaaa", "bbbb", "cc".
        history.record(&lines(&["first", "aaaabbbbcc", "last"]), 4, 8);

        assert_eq!(text(&history.newest_rows(3)), vec!["bbbbcc", "last"]);
        assert_eq!(text(&history.newest_rows(4)), vec!["aaaabbbbcc", "last"]);
    }

    #[test]
    fn clipping_a_soft_wrapped_wide_grapheme_keeps_the_next_row() {
        let mut history = WrittenHistory::default();
        history.record(&lines(&["abc界xy"]), 4, 8);
        assert_eq!(text(&history.newest_rows(1)), vec!["界xy"]);
    }
}
