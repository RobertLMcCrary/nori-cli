//! Parsing a whole transcript file at once.
//!
//! A transcript is JSON lines. Finding the lines is a `memchr` scan, and the lines parse
//! independently, so a large transcript is parsed on every core at once. The entries keep their
//! order in the file, and the result is exactly what parsing it line by line gives.

use std::ops::Range;

use super::types::SessionMetaEntry;
use super::types::TranscriptEntry;
use super::types::TranscriptLine;

/// Transcripts smaller than this are parsed on the calling thread; starting threads would cost
/// more than they save.
const PARALLEL_THRESHOLD_BYTES: usize = 256 * 1024;

pub(super) struct ParsedTranscript {
    pub(super) meta: SessionMetaEntry,
    pub(super) entries: Vec<TranscriptLine>,
    pub(super) line_count: usize,
    pub(super) skipped_count: usize,
}

#[derive(Debug)]
pub(super) enum ParseFailure {
    /// A line before the session metadata did not parse.
    BeforeMeta(serde_json::Error),
    /// No line carried the session metadata.
    NoMeta,
}

/// The byte ranges of the lines of `bytes`, without their line endings.
fn line_ranges(bytes: &[u8]) -> Vec<Range<usize>> {
    let mut ranges = Vec::with_capacity(bytes.len() / 256 + 1);
    let mut start = 0;
    for newline in memchr::memchr_iter(b'\n', bytes) {
        ranges.push(start..newline);
        start = newline + 1;
    }
    if start < bytes.len() {
        ranges.push(start..bytes.len());
    }
    for range in &mut ranges {
        if bytes[range.clone()].last() == Some(&b'\r') {
            range.end -= 1;
        }
    }
    ranges
}

/// `None` for a blank line, which is not an entry.
fn parse_line(line: &[u8]) -> Option<Result<TranscriptLine, serde_json::Error>> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    Some(serde_json::from_slice(line))
}

/// Parse every line of a transcript. Lines that do not parse after the session metadata (entry
/// kinds from other versions) are skipped; one that does not parse before it fails the load.
pub(super) fn parse_transcript(bytes: &[u8]) -> Result<ParsedTranscript, ParseFailure> {
    let ranges = line_ranges(bytes);
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let parsed: Vec<Option<Result<TranscriptLine, serde_json::Error>>> =
        if bytes.len() < PARALLEL_THRESHOLD_BYTES || threads < 2 {
            ranges
                .iter()
                .map(|range| parse_line(&bytes[range.clone()]))
                .collect()
        } else {
            let per_thread = ranges.len().div_ceil(threads).max(1);
            std::thread::scope(|scope| {
                #[expect(
                    clippy::needless_collect,
                    reason = "every worker is spawned before the first is joined"
                )]
                let workers: Vec<_> = ranges
                    .chunks(per_thread)
                    .map(|chunk| {
                        scope.spawn(move || {
                            chunk
                                .iter()
                                .map(|range| parse_line(&bytes[range.clone()]))
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .flat_map(|worker| {
                        worker
                            .join()
                            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                    })
                    .collect()
            })
        };

    let mut meta: Option<SessionMetaEntry> = None;
    let mut entries = Vec::with_capacity(parsed.len());
    let mut skipped_count = 0;
    for line in parsed.into_iter().flatten() {
        match line {
            Ok(entry) => {
                if meta.is_none()
                    && let TranscriptEntry::SessionMeta(session_meta) = &entry.entry
                {
                    meta = Some(session_meta.clone());
                }
                entries.push(entry);
            }
            Err(error) if meta.is_none() => return Err(ParseFailure::BeforeMeta(error)),
            Err(error) => {
                skipped_count += 1;
                tracing::debug!(%error, "skipping unrecognized transcript entry");
            }
        }
    }
    Ok(ParsedTranscript {
        meta: meta.ok_or(ParseFailure::NoMeta)?,
        entries,
        line_count: ranges.len(),
        skipped_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn meta_line() -> String {
        serde_json::json!({
            "ts": "2026-01-01T00:00:00Z",
            "v": 1,
            "type": "session_meta",
            "session_id": "s",
            "project_id": "p",
            "started_at": "2026-01-01T00:00:00Z",
            "cwd": "/work",
            "cli_version": "0.0.0"
        })
        .to_string()
    }

    fn user_line(index: usize) -> String {
        serde_json::json!({
            "ts": "2026-01-01T00:00:01Z",
            "v": 1,
            "type": "user",
            "id": format!("u{index}"),
            "content": format!("prompt number {index}")
        })
        .to_string()
    }

    #[test]
    fn lines_split_on_newlines_and_drop_carriage_returns() {
        let text = b"a\r\nb\n\nc";
        let ranges = line_ranges(text);
        let lines: Vec<&[u8]> = ranges.iter().map(|range| &text[range.clone()]).collect();
        assert_eq!(lines, vec![&b"a"[..], b"b", b"", b"c"]);
    }

    #[test]
    fn a_large_transcript_parses_in_parallel_in_file_order() {
        let mut text = meta_line();
        text.push('\n');
        for index in 0..20_000 {
            text.push_str(&user_line(index));
            text.push('\n');
            if index % 1000 == 0 {
                text.push_str("{\"type\": \"from_a_future_version\"}\n\n");
            }
        }
        assert!(text.len() > PARALLEL_THRESHOLD_BYTES);
        let parsed = parse_transcript(text.as_bytes()).expect("parses");
        assert_eq!(parsed.entries.len(), 20_001);
        assert_eq!(parsed.skipped_count, 20);
        let first_ids: Vec<String> = parsed.entries[1..4]
            .iter()
            .map(|entry| match &entry.entry {
                TranscriptEntry::User(user) => user.id.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(first_ids, vec!["u0", "u1", "u2"]);
        assert_eq!(parsed.meta.session_id, "s");
    }

    #[test]
    fn a_bad_line_before_the_metadata_fails_the_load() {
        let text = format!("not json\n{}\n", meta_line());
        assert!(matches!(
            parse_transcript(text.as_bytes()),
            Err(ParseFailure::BeforeMeta(_))
        ));
        assert!(matches!(
            parse_transcript(user_line(0).as_bytes()),
            Err(ParseFailure::NoMeta)
        ));
    }
}
