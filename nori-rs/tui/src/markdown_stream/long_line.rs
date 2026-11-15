//! The rows of a long paragraph line that are final while it is still being typed.
//!
//! A paragraph line is shown live until its newline arrives, because what comes later can change
//! how it renders. For an answer that is one paragraph of a thousand rows, that means none of it
//! reaches the scrollback, and most of it is not on screen either, until the turn ends. But most
//! of those rows cannot change any more, and they are committed as soon as that is certain.
//!
//! Two things could still change a row: the inline markup (a `*` or a `` ` `` that closes later
//! restyles what came before it) and the wrapping. Markup is ruled out by committing only from
//! the plain part of the line, the part before its first character that markup, a URL or a
//! different word splitting could make anything of. The wrapping is ruled out by the renderer:
//! a line longer than [`LONG_LINE_ROWS`] rows is wrapped first-fit
//! (`markdown_render::LONG_LINE_ROWS`), which places a row's break by the row's own words and
//! the next word alone. So once the first word of the next row is complete and plain, a row is
//! what it will be when the message is complete, and the one-shot render of the message has it
//! too.

use ratatui::text::Line;

use crate::markdown_render::LONG_LINE_ROWS;

/// How many of `live` (the rows of the line being typed that were not committed, rendered at
/// `width`) are final, and where in `buffer` the first row after them starts.
///
/// `line_start` is where the line starts in `buffer`, `block_start` where its block does, and
/// `committed_to` where the first row of `live` starts.
pub(super) fn final_rows(
    buffer: &str,
    block_start: usize,
    line_start: usize,
    committed_to: usize,
    live: &[Line<'static>],
    width: usize,
) -> Option<(usize, usize)> {
    // A paragraph, at the top level, whose line starts as plain text: no prefix on its rows.
    let starts_plain = |text: &str| text.starts_with(|c: char| c.is_ascii_alphabetic());
    if !starts_plain(buffer[block_start..].trim_start()) || !starts_plain(&buffer[line_start..]) {
        return None;
    }
    let line = &buffer[line_start..];
    let mut plain = line
        .bytes()
        .position(|byte| !is_plain(byte))
        .unwrap_or(line.len());
    // A word too long for a row is split in a way a URL later on the line would undo.
    let mut word_start = 0;
    for (index, byte) in line.as_bytes()[..plain].iter().enumerate() {
        if *byte == b' ' {
            word_start = index + 1;
        } else if index + 1 - word_start > width {
            plain = word_start;
            break;
        }
    }
    // Only a line that is long once it is complete is wrapped first-fit: this much of it already.
    if plain <= LONG_LINE_ROWS * width {
        return None;
    }

    let mut offset = committed_to - line_start;
    let mut rows = 0;
    for pair in live.windows(2) {
        let (row, next) = (pair[0].to_string(), pair[1].to_string());
        let start = offset + spaces(&line[offset..]);
        if !line[start..].starts_with(&row) {
            break;
        }
        let row_end = start + row.len();
        let next_start = row_end + spaces(&line[row_end..]);
        let first_word_end = next_start + next.split(' ').next().map_or(0, str::len);
        // The next row's first word decided where this row ends; it must be all there.
        if first_word_end >= plain || line.as_bytes()[first_word_end] != b' ' {
            break;
        }
        rows += 1;
        offset = row_end;
    }
    (rows > 0).then_some((rows, line_start + offset))
}

/// A byte of text that nothing later on its line can make anything other than itself, and that
/// a row cannot break at: no markup, link, entity or escape character, and none that the word
/// splitting around URLs treats differently.
fn is_plain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'.' | b',' | b';' | b':' | b'\'' | b'"')
}

fn spaces(text: &str) -> usize {
    text.bytes().take_while(|byte| *byte == b' ').count()
}
