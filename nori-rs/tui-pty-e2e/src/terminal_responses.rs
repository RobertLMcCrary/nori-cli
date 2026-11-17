//! Answer terminal cursor reports using the emulator state at the query position.

use std::io::Write;
use std::io::{self};

use vt100::Parser;

pub(super) fn process(
    parser: &mut Parser,
    writer: &mut impl Write,
    pending: &mut Vec<u8>,
    data: &[u8],
) -> io::Result<()> {
    const QUERY: &[u8] = b"\x1b[6n";
    let mut bytes = std::mem::take(pending);
    bytes.extend_from_slice(data);
    let mut start = 0;
    while let Some(relative) = bytes[start..]
        .windows(QUERY.len())
        .position(|window| window == QUERY)
    {
        let at = start + relative;
        parser.process(&bytes[start..at]);
        let (row, column) = parser.screen().cursor_position();
        write!(
            writer,
            "\x1b[{};{}R",
            u32::from(row) + 1,
            u32::from(column) + 1
        )?;
        writer.flush()?;
        start = at + QUERY.len();
    }
    let remainder = &bytes[start..];
    let retained = (1..QUERY.len())
        .rev()
        .find(|count| {
            remainder.len() >= *count && QUERY.starts_with(&remainder[remainder.len() - *count..])
        })
        .unwrap_or(0);
    parser.process(&remainder[..remainder.len() - retained]);
    pending.extend_from_slice(&remainder[remainder.len() - retained..]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn answers_each_query_after_processing_its_preceding_output() {
        let mut parser = Parser::new(25, 80, 0);
        let mut replies = Vec::new();
        let mut pending = Vec::new();
        process(
            &mut parser,
            &mut replies,
            &mut pending,
            b"\x1b[5;9Habc\x1b[6n\x1b[3;2H\x1b[6n",
        )
        .unwrap();
        assert_eq!(replies, b"\x1b[5;12R\x1b[3;2R");
        assert!(pending.is_empty());
    }

    #[test]
    fn handles_split_queries_and_unrelated_escape_sequences() {
        let mut parser = Parser::new(25, 80, 0);
        let mut replies = Vec::new();
        let mut pending = Vec::new();
        process(&mut parser, &mut replies, &mut pending, b"\x1b[4;6H\x1b[6").unwrap();
        assert!(replies.is_empty());
        process(&mut parser, &mut replies, &mut pending, b"n\x1b[").unwrap();
        process(&mut parser, &mut replies, &mut pending, b"2;3H\x1b[6n").unwrap();
        assert_eq!(replies, b"\x1b[4;6R\x1b[2;3R");
        assert!(pending.is_empty());
    }
}
