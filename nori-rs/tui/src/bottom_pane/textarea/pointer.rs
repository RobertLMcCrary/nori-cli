//! Map a rendered textarea cell to a valid text cursor position.
use std::ops::Range;

use ratatui::layout::Rect;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::TextArea;
use super::TextAreaState;

impl TextArea {
    pub(crate) fn place_cursor_at(
        &mut self,
        area: Rect,
        state: TextAreaState,
        column: u16,
        row: u16,
    ) -> bool {
        if area.is_empty()
            || column < area.x
            || column >= area.right()
            || row < area.y
            || row >= area.bottom()
        {
            return false;
        }
        let x = usize::from(column - area.x);
        let y = usize::from(row - area.y);
        let (lines, first) = if self.kakoune_mode_enabled {
            let lines = modal_rows(&self.text, usize::from(area.width));
            let cursor_row =
                Self::wrapped_line_index_by_start(&lines, self.cursor_pos).unwrap_or(0);
            let screen_row = self
                .kakoune_editor
                .as_ref()
                .and_then(super::KakouneEditor::term_cursor)
                .map(|(_, cursor_y)| usize::from(cursor_y.saturating_sub(area.y)))
                .unwrap_or(0);
            (lines, cursor_row.saturating_sub(screen_row))
        } else {
            let lines = self.wrapped_lines(area.width);
            let first = usize::from(self.effective_scroll(area.height, &lines, state.scroll));
            (lines.clone(), first)
        };
        let line = &lines[(first + y).min(lines.len() - 1)];
        let start = line.start.min(self.text.len());
        let end = line.end.min(self.text.len());
        let mut position = end;
        let mut width = 0;
        for (offset, grapheme) in self.text[start..end].grapheme_indices(true) {
            let next = width + grapheme.width();
            if x < next {
                position = start + offset;
                break;
            }
            width = next;
        }
        self.set_cursor(position);
        true
    }
}

/// Modalkit's textbox wraps by cell width rather than at word boundaries.
fn modal_rows(text: &str, width: usize) -> Vec<Range<usize>> {
    let mut rows = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (offset, grapheme) in text.grapheme_indices(true) {
        if grapheme == "\n" {
            rows.push(start..offset);
            start = offset + grapheme.len();
            used = 0;
            continue;
        }
        let cells = grapheme.width();
        if used > 0 && used + cells > width {
            rows.push(start..offset);
            start = offset;
            used = 0;
        }
        used += cells;
    }
    rows.push(start..text.len());
    if used == width && width > 0 {
        rows.push(text.len()..text.len());
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use pretty_assertions::assert_eq;
    use ratatui::buffer::Buffer;
    use ratatui::widgets::WidgetRef;

    #[test]
    fn tapping_wrapped_unicode_moves_modal_cursor_without_changing_mode() {
        let mut textarea = TextArea::new();
        textarea.set_kakoune_mode_enabled(true);
        for ch in "abé🙂xy".chars() {
            textarea.input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let area = Rect::new(3, 2, 5, 3);
        let mut buffer = Buffer::empty(area);
        WidgetRef::render_ref(&&textarea, area, &mut buffer);
        assert!(textarea.place_cursor_at(area, TextAreaState::default(), 3, 3));
        textarea.input(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        assert_eq!(textarea.text(), "abé🙂!xy");
        assert_eq!(
            textarea.vim_mode_state(),
            super::super::VimModeState::Insert
        );
        let mut after = Buffer::empty(area);
        WidgetRef::render_ref(&&textarea, area, &mut after);
        let lines = (area.top()..area.bottom())
            .map(|y| {
                (area.left()..area.right())
                    .map(|x| after[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!("modal_pointer_wrapped_unicode", lines);
        assert!(!textarea.place_cursor_at(area, TextAreaState::default(), 2, 2));
    }

    #[test]
    fn tapping_scrolled_modal_text_uses_visible_rows() {
        let mut textarea = TextArea::new();
        textarea.set_kakoune_mode_enabled(true);
        for ch in "abcde12345uvwxy".chars() {
            textarea.input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let area = Rect::new(0, 0, 5, 2);
        let mut buffer = Buffer::empty(area);
        WidgetRef::render_ref(&&textarea, area, &mut buffer);
        let row: String = (0..5).map(|x| buffer[(x, 0)].symbol()).collect();
        assert_eq!(row, "uvwxy");
        assert!(textarea.place_cursor_at(area, TextAreaState::default(), 0, 0));
        textarea.input(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        assert_eq!(textarea.text(), "abcde12345!uvwxy");
    }
}

#[cfg(test)]
mod standard_tests {
    use super::*;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use pretty_assertions::assert_eq;

    #[test]
    fn tapping_standard_editor_uses_grapheme_boundaries() {
        let mut textarea = TextArea::new();
        textarea.insert_str("aé🙂xy");
        let area = Rect::new(1, 1, 10, 2);
        assert!(textarea.place_cursor_at(area, TextAreaState::default(), 3, 1));
        textarea.input(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        assert_eq!(textarea.text(), "aé!🙂xy");
    }
}
