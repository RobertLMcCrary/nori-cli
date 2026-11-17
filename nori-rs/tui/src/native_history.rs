//! Geometry for logical lines written directly to the terminal's native history.
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Count terminal rows without inserting hard line breaks. A wide grapheme
/// that does not fit at the end of a row begins on the next row.
pub(crate) fn physical_rows(line: &Line<'_>, width: usize) -> usize {
    let width = width.max(1);
    let text = line.to_string();
    let mut rows = 1;
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        let cells = grapheme.width().min(width);
        if cells == 0 {
            continue;
        }
        if column + cells > width {
            rows += 1;
            column = 0;
        }
        column += cells;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    #[test]
    fn counts_wide_characters_at_the_edge_and_exact_width() {
        assert_eq!(physical_rows(&Line::from("abcd"), 4), 1);
        assert_eq!(physical_rows(&Line::from("abc界x"), 4), 2);
        assert_eq!(physical_rows(&Line::from("abc界xyz"), 4), 3);
        assert_eq!(physical_rows(&Line::from(""), 4), 1);
    }
}
