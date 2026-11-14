use std::path::PathBuf;

use ratatui::text::Line;

use crate::markdown_render::committable_prefix_len;
use crate::markdown_render::markdown_segments;
use crate::markdown_render::render_markdown_continuation;
use crate::markdown_render::top_level_block_starts;
use crate::render::line_utils::is_blank_line_spaces_only;

mod fence;

pub(crate) use fence::FenceOpener;

/// Incremental markdown renderer for a message that is still streaming.
///
/// Completed top-level blocks are frozen: rendered once, their lines handed out, and their source
/// never parsed again. Only the open block (the last one, which more text can still change) is
/// re-rendered, so handling a delta costs the delta plus the open block, not the message.
///
/// Within the open block, lines that end in a newline are committed (handed out once, for the
/// scrollback), and the rest are the live tail: what the line being typed currently looks like.
///
/// A fenced code block is streamed by [`fence::OpenFence`] instead: each line is highlighted and
/// committed once, so a long code block costs its length in total rather than on every frame.
pub(crate) struct MarkdownStreamCollector {
    buffer: String,
    width: Option<usize>,
    cwd: Option<PathBuf>,
    /// Bytes of `buffer` that are frozen. The open block starts here.
    frozen_len: usize,
    /// The frozen text ends inside a prose segment, so the open block continues it.
    prose_continues: bool,
    /// Lines of the open block already handed out.
    committed_in_open: usize,
    /// Length of the open block's complete prefix that was last rendered.
    complete_cache: Option<usize>,
    live: Vec<Line<'static>>,
    dirty: bool,
    /// The open block is a fenced code block streamed line by line.
    fence: Option<fence::OpenFence>,
    /// Bytes handed to the markdown parser so far, for work-counting tests.
    parsed_bytes: usize,
    /// Bytes highlighted by fences that have closed, for work-counting tests.
    highlighted_bytes: usize,
}

impl MarkdownStreamCollector {
    pub fn new(width: Option<usize>) -> Self {
        Self::with_cwd(width, None)
    }

    #[allow(dead_code)]
    pub fn new_with_cwd(width: Option<usize>, cwd: PathBuf) -> Self {
        Self::with_cwd(width, Some(cwd))
    }

    fn with_cwd(width: Option<usize>, cwd: Option<PathBuf>) -> Self {
        Self {
            buffer: String::new(),
            width,
            cwd,
            frozen_len: 0,
            prose_continues: false,
            committed_in_open: 0,
            complete_cache: None,
            live: Vec::new(),
            dirty: false,
            fence: None,
            parsed_bytes: 0,
            highlighted_bytes: 0,
        }
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.frozen_len = 0;
        self.prose_continues = false;
        self.committed_in_open = 0;
        self.complete_cache = None;
        self.live.clear();
        self.dirty = false;
        self.fence = None;
    }

    /// Append a delta. Costs the length of the delta; rendering waits for [`Self::advance`].
    pub fn push_delta(&mut self, delta: &str) {
        self.buffer.push_str(delta);
        self.dirty = true;
    }

    /// The line being typed and anything else rendered but not yet committed, as of the last
    /// [`Self::advance`].
    pub fn live_tail(&self) -> &[Line<'static>] {
        &self.live
    }

    /// Whether a delta arrived that [`Self::advance`] has not rendered yet.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Bytes handed to the markdown parser so far.
    #[cfg(test)]
    pub fn parsed_bytes(&self) -> usize {
        self.parsed_bytes
    }

    /// Bytes of fenced code highlighted so far.
    #[cfg(test)]
    pub fn highlighted_bytes(&self) -> usize {
        self.highlighted_bytes
            + self
                .fence
                .as_ref()
                .map_or(0, |fence| fence.highlighted_bytes)
    }

    /// Render `source` and count the bytes handed to the parser.
    fn render(&mut self, range: std::ops::Range<usize>, preceded: bool) -> Vec<Line<'static>> {
        self.parsed_bytes += range.len();
        render_markdown_continuation(
            &self.buffer[range],
            self.width,
            self.cwd.as_deref(),
            preceded,
        )
    }

    /// Render everything pushed since the last call. Returns the lines that became final since
    /// then, oldest first, and refreshes the live tail.
    ///
    /// Rendering is append-only for the committed prefix except for markdown tables, whose column
    /// widths depend on every row: an unfinished trailing table is withheld (see
    /// [`committable_prefix_len`]) and shown live instead.
    pub fn advance(&mut self) -> Vec<Line<'static>> {
        if !self.dirty {
            return Vec::new();
        }
        self.dirty = false;
        let mut out = Vec::new();
        loop {
            if self.fence.is_none() && !self.open_fence(&mut out) {
                self.freeze_completed_blocks(&mut out);
                if !self.open_fence(&mut out) {
                    break;
                }
            }
            let Some(fence) = self.fence.as_mut() else {
                break;
            };
            match fence.advance(&self.buffer, &mut out) {
                fence::FenceProgress::Open => {
                    self.live = fence.live(&self.buffer);
                    return out;
                }
                fence::FenceProgress::Closed { end } => self.close_fence(end),
            }
        }

        let open_start = self.frozen_len;
        let preceded = self.prose_continues;
        let open_len = self.buffer.len() - open_start;
        let complete_len = match memchr::memrchr(b'\n', &self.buffer.as_bytes()[open_start..]) {
            Some(newline) => {
                committable_prefix_len(&self.buffer[open_start..=open_start + newline])
            }
            None => 0,
        };

        if complete_len > 0 && self.complete_cache.is_none_or(|len| len != complete_len) {
            let rendered = self.render(open_start..open_start + complete_len, preceded);
            let mut count = rendered.len();
            if count > 0 && is_blank_line_spaces_only(&rendered[count - 1]) {
                count -= 1;
            }
            if count > self.committed_in_open {
                out.extend(rendered[self.committed_in_open..count].iter().cloned());
                self.committed_in_open = count;
            }
            self.complete_cache = Some(complete_len);
        }

        // The live tail is the open block as it stands now, past what was committed. Nothing is
        // being typed when every byte of it is a committed line.
        let mut live: Vec<Line<'static>> = if complete_len == open_len {
            Vec::new()
        } else {
            self.render(open_start..self.buffer.len(), preceded)
                .into_iter()
                .skip(self.committed_in_open)
                .collect()
        };
        while live.last().is_some_and(is_blank_line_spaces_only) {
            live.pop();
        }
        self.live = live;
        out
    }

    /// Start streaming the open block as a fenced code block if it is one: its first line with
    /// content is a complete fence opener. The block's leading separator is rendered by the
    /// renderer, so it is exactly what rendering the whole message gives.
    fn open_fence(&mut self, out: &mut Vec<Line<'static>>) -> bool {
        let open = &self.buffer[self.frozen_len..];
        let mut offset = 0;
        for line in open.split_inclusive('\n') {
            let Some(content) = line.strip_suffix('\n') else {
                return false;
            };
            offset += line.len();
            if content.trim().is_empty() {
                continue;
            }
            let Some(opener) = FenceOpener::parse(content) else {
                return false;
            };
            let body_start = self.frozen_len + offset;
            let opening = self.render(self.frozen_len..body_start, self.prose_continues);
            out.extend(opening.into_iter().skip(self.committed_in_open));
            self.committed_in_open = 0;
            self.complete_cache = None;
            self.fence = Some(fence::OpenFence::new(opener, body_start));
            return true;
        }
        false
    }

    /// The open fence closed at `end`: the next block starts after it, preceded by it.
    fn close_fence(&mut self, end: usize) {
        if let Some(fence) = self.fence.take() {
            self.highlighted_bytes += fence.highlighted_bytes;
        }
        self.frozen_len = end;
        self.prose_continues = true;
        self.committed_in_open = 0;
        self.complete_cache = None;
        self.live.clear();
    }

    /// Freeze every block that a later block has closed: render it, hand out its lines, and move
    /// the open block's start past it.
    fn freeze_completed_blocks(&mut self, out: &mut Vec<Line<'static>>) {
        let tail = &self.buffer[self.frozen_len..];
        // Only complete lines decide block structure; the line being typed can still change it.
        let Some(stable_end) = memchr::memrchr(b'\n', tail.as_bytes()).map(|newline| newline + 1)
        else {
            return;
        };
        let stable = &tail[..stable_end];
        let segments = markdown_segments(stable);
        let Some(last) = segments.last() else {
            return;
        };

        // Where the open block starts, relative to the tail.
        let mut open_start = last.range.start;
        let mut open_continues_prose = false;
        if !last.is_table {
            let starts = top_level_block_starts(&stable[last.range.clone()]);
            self.parsed_bytes += last.range.len();
            if starts.len() >= 2 {
                open_start = last.range.start + starts[starts.len() - 1];
                open_continues_prose = true;
            }
        }
        if open_start == 0 {
            return;
        }

        let from = self.frozen_len;
        let frozen = self.render(from..from + open_start, self.prose_continues);
        // The block that was open is the first thing frozen; its early lines were already
        // handed out as they were committed.
        out.extend(frozen.into_iter().skip(self.committed_in_open));
        self.frozen_len += open_start;
        self.prose_continues = open_continues_prose;
        self.committed_in_open = 0;
        self.complete_cache = None;
    }

    /// Finalize the stream: hand out everything still pending. If the buffer does not end with a
    /// newline, a temporary one is appended for rendering. Returns the raw source unchanged.
    pub fn finalize_and_drain(&mut self) -> (Vec<Line<'static>>, String) {
        self.dirty = true;
        let mut out = self.advance();
        if let Some(fence) = self.fence.as_mut() {
            // The message ended inside a code block, which ends it.
            fence.finish(&self.buffer, &mut out);
            let end = self.buffer.len();
            self.close_fence(end);
        }

        let mut appended_newline = false;
        if self.frozen_len < self.buffer.len() {
            if !self.buffer.ends_with('\n') {
                self.buffer.push('\n');
                appended_newline = true;
            }
            tracing::debug!(raw_len = self.buffer.len(), "markdown finalize");
            let rendered = self.render(self.frozen_len..self.buffer.len(), self.prose_continues);
            out.extend(rendered.into_iter().skip(self.committed_in_open));
        }

        let mut raw = std::mem::take(&mut self.buffer);
        // The temporary newline is not part of the message.
        if appended_newline {
            raw.pop();
        }
        self.clear();
        (out, raw)
    }

    /// Advance and return only the newly committed lines. A convenience for tests that commit
    /// after each delta.
    #[cfg(test)]
    pub fn commit_complete_lines(&mut self) -> Vec<Line<'static>> {
        self.dirty = true;
        self.advance()
    }
}

#[cfg(test)]
mod incremental_tests;

#[cfg(test)]
pub(crate) fn simulate_stream_markdown_for_tests(
    deltas: &[&str],
    finalize: bool,
) -> Vec<Line<'static>> {
    let mut collector = MarkdownStreamCollector::new(None);
    let mut out = Vec::new();
    for d in deltas {
        collector.push_delta(d);
        if d.contains('\n') {
            out.extend(collector.commit_complete_lines());
        }
    }
    if finalize {
        out.extend(collector.finalize_and_drain().0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[tokio::test]
    async fn no_commit_until_newline() {
        let mut c = super::MarkdownStreamCollector::new(None);
        c.push_delta("Hello, world");
        let out = c.commit_complete_lines();
        assert!(out.is_empty(), "should not commit without newline");
        c.push_delta("!\n");
        let out2 = c.commit_complete_lines();
        assert_eq!(out2.len(), 1, "one completed line after newline");
    }

    #[tokio::test]
    async fn finalize_commits_partial_line() {
        let mut c = super::MarkdownStreamCollector::new(None);
        c.push_delta("Line without newline");
        let (out, _source) = c.finalize_and_drain();
        assert_eq!(out.len(), 1);
    }

    #[tokio::test]
    async fn e2e_stream_blockquote_simple_is_green() {
        let out = super::simulate_stream_markdown_for_tests(&["> Hello\n"], true);
        assert_eq!(out.len(), 1);
        let l = &out[0];
        assert_eq!(
            l.style.fg,
            Some(Color::Green),
            "expected blockquote line fg green, got {:?}",
            l.style.fg
        );
    }

    #[tokio::test]
    async fn e2e_stream_blockquote_nested_is_green() {
        let out = super::simulate_stream_markdown_for_tests(&["> Level 1\n>> Level 2\n"], true);
        // Filter out any blank lines that may be inserted at paragraph starts.
        let non_blank: Vec<_> = out
            .into_iter()
            .filter(|l| {
                let s = l
                    .spans
                    .iter()
                    .map(|sp| sp.content.clone())
                    .collect::<Vec<_>>()
                    .join("");
                let t = s.trim();
                // Ignore quote-only blank lines like ">" inserted at paragraph boundaries.
                !(t.is_empty() || t == ">")
            })
            .collect();
        assert_eq!(non_blank.len(), 2);
        assert_eq!(non_blank[0].style.fg, Some(Color::Green));
        assert_eq!(non_blank[1].style.fg, Some(Color::Green));
    }

    #[tokio::test]
    async fn e2e_stream_blockquote_with_list_items_is_green() {
        let out = super::simulate_stream_markdown_for_tests(&["> - item 1\n> - item 2\n"], true);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].style.fg, Some(Color::Green));
        assert_eq!(out[1].style.fg, Some(Color::Green));
    }

    #[tokio::test]
    async fn e2e_stream_nested_mixed_lists_ordered_marker_is_light_blue() {
        let md = [
            "1. First\n",
            "   - Second level\n",
            "     1. Third level (ordered)\n",
            "        - Fourth level (bullet)\n",
            "          - Fifth level to test indent consistency\n",
        ];
        let out = super::simulate_stream_markdown_for_tests(&md, true);
        // Find the line that contains the third-level ordered text
        let find_idx = out.iter().position(|l| {
            l.spans
                .iter()
                .map(|s| s.content.clone())
                .collect::<String>()
                .contains("Third level (ordered)")
        });
        let idx = find_idx.expect("expected third-level ordered line");
        let line = &out[idx];
        // Expect at least one span on this line to be styled light blue
        let has_light_blue = line
            .spans
            .iter()
            .any(|s| s.style.fg == Some(ratatui::style::Color::LightBlue));
        assert!(
            has_light_blue,
            "expected an ordered-list marker span with light blue fg on: {line:?}"
        );
    }

    #[tokio::test]
    async fn e2e_stream_blockquote_wrap_preserves_green_style() {
        let long = "> This is a very long quoted line that should wrap across multiple columns to verify style preservation.";
        let out = super::simulate_stream_markdown_for_tests(&[long, "\n"], true);
        // Wrap to a narrow width to force multiple output lines.
        let wrapped =
            crate::wrapping::word_wrap_lines(out.iter(), crate::wrapping::RtOptions::new(24));
        // Filter out purely blank lines
        let non_blank: Vec<_> = wrapped
            .into_iter()
            .filter(|l| {
                let s = l
                    .spans
                    .iter()
                    .map(|sp| sp.content.clone())
                    .collect::<Vec<_>>()
                    .join("");
                !s.trim().is_empty()
            })
            .collect();
        assert!(
            non_blank.len() >= 2,
            "expected wrapped blockquote to span multiple lines"
        );
        for (i, l) in non_blank.iter().enumerate() {
            assert_eq!(
                l.spans[0].style.fg,
                Some(Color::Green),
                "wrapped line {} should preserve green style, got {:?}",
                i,
                l.spans[0].style.fg
            );
        }
    }

    #[tokio::test]
    async fn heading_starts_on_new_line_when_following_paragraph() {
        // Stream a paragraph line, then a heading on the next line.
        // Expect two distinct rendered lines: "Hello." and "Heading".
        let mut c = super::MarkdownStreamCollector::new(None);
        c.push_delta("Hello.\n");
        let out1 = c.commit_complete_lines();
        let s1: Vec<String> = out1
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        assert_eq!(
            out1.len(),
            1,
            "first commit should contain only the paragraph line, got {}: {:?}",
            out1.len(),
            s1
        );

        c.push_delta("## Heading\n");
        let out2 = c.commit_complete_lines();
        let s2: Vec<String> = out2
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        assert_eq!(
            s2,
            vec!["", "## Heading"],
            "expected a blank separator then the heading line"
        );

        let line_to_string = |l: &ratatui::text::Line<'_>| -> String {
            l.spans
                .iter()
                .map(|s| s.content.clone())
                .collect::<Vec<_>>()
                .join("")
        };

        assert_eq!(line_to_string(&out1[0]), "Hello.");
        assert_eq!(line_to_string(&out2[1]), "## Heading");
    }

    #[tokio::test]
    async fn heading_not_inlined_when_split_across_chunks() {
        // Paragraph without trailing newline, then a chunk that starts with the newline
        // and the heading text, then a final newline. The collector should first commit
        // only the paragraph line, and later commit the heading as its own line.
        let mut c = super::MarkdownStreamCollector::new(None);
        c.push_delta("Sounds good!");
        // No commit yet
        assert!(c.commit_complete_lines().is_empty());

        // Introduce the newline that completes the paragraph and the start of the heading.
        c.push_delta("\n## Adding Bird subcommand");
        let out1 = c.commit_complete_lines();
        let s1: Vec<String> = out1
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        assert_eq!(
            s1,
            vec!["Sounds good!"],
            "expected paragraph followed by blank separator before heading chunk"
        );

        // Now finish the heading line with the trailing newline.
        c.push_delta("\n");
        let out2 = c.commit_complete_lines();
        let s2: Vec<String> = out2
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        assert_eq!(
            s2,
            vec!["", "## Adding Bird subcommand"],
            "expected the heading line only on the final commit"
        );

        // Sanity check raw markdown rendering for a simple line does not produce spurious extras.
        let mut rendered: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown("Hello.\n", None, &mut rendered);
        let rendered_strings: Vec<String> = rendered
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        assert_eq!(
            rendered_strings,
            vec!["Hello."],
            "unexpected markdown lines: {rendered_strings:?}"
        );
    }

    fn lines_to_plain_strings(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    #[tokio::test]
    async fn lists_and_fences_commit_without_duplication() {
        // List case
        assert_streamed_equals_full(&["- a\n- ", "b\n- c\n"]).await;

        // Fenced code case: stream in small chunks
        assert_streamed_equals_full(&["```", "\nco", "de 1\ncode 2\n", "```\n"]).await;
    }

    #[tokio::test]
    async fn utf8_boundary_safety_and_wide_chars() {
        // Emoji (wide), CJK, control char, digit + combining macron sequences
        let input = "🙂🙂🙂\n汉字漢字\nA\u{0003}0\u{0304}\n";
        let deltas = vec![
            "🙂",
            "🙂",
            "🙂\n汉",
            "字漢",
            "字\nA",
            "\u{0003}",
            "0",
            "\u{0304}",
            "\n",
        ];

        let streamed = simulate_stream_markdown_for_tests(&deltas, true);
        let streamed_str = lines_to_plain_strings(&streamed);

        let mut rendered_all: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown(input, None, &mut rendered_all);
        let rendered_all_str = lines_to_plain_strings(&rendered_all);

        assert_eq!(
            streamed_str, rendered_all_str,
            "utf8/wide-char streaming should equal full render without duplication or truncation"
        );
    }

    #[tokio::test]
    async fn e2e_stream_deep_nested_third_level_marker_is_light_blue() {
        let md = "1. First\n   - Second level\n     1. Third level (ordered)\n        - Fourth level (bullet)\n          - Fifth level to test indent consistency\n";
        let streamed = super::simulate_stream_markdown_for_tests(&[md], true);
        let streamed_strs = lines_to_plain_strings(&streamed);

        // Locate the third-level line in the streamed output; avoid relying on exact indent.
        let target_suffix = "1. Third level (ordered)";
        let mut found = None;
        for line in &streamed {
            let s: String = line.spans.iter().map(|sp| sp.content.clone()).collect();
            if s.contains(target_suffix) {
                found = Some(line.clone());
                break;
            }
        }
        let line = found.unwrap_or_else(|| {
            panic!("expected to find the third-level ordered list line; got: {streamed_strs:?}")
        });

        // The marker (including indent and "1.") is expected to be in the first span
        // and colored LightBlue; following content should be default color.
        assert!(
            !line.spans.is_empty(),
            "expected non-empty spans for the third-level line"
        );
        let marker_span = &line.spans[0];
        assert_eq!(
            marker_span.style.fg,
            Some(Color::LightBlue),
            "expected LightBlue 3rd-level ordered marker, got {:?}",
            marker_span.style.fg
        );
        // Find the first non-empty non-space content span and verify it is default color.
        let mut content_fg = None;
        for sp in &line.spans[1..] {
            let t = sp.content.trim();
            if !t.is_empty() {
                content_fg = Some(sp.style.fg);
                break;
            }
        }
        assert_eq!(
            content_fg.flatten(),
            None,
            "expected default color for 3rd-level content, got {content_fg:?}"
        );
    }

    #[tokio::test]
    async fn empty_fenced_block_is_dropped_and_separator_preserved_before_heading() {
        // An empty fenced code block followed by a heading should not render the fence,
        // but should preserve a blank separator line so the heading starts on a new line.
        let deltas = vec!["```bash\n```\n", "## Heading\n"]; // empty block and close in same commit
        let streamed = simulate_stream_markdown_for_tests(&deltas, true);
        let texts = lines_to_plain_strings(&streamed);
        assert!(
            texts.iter().all(|s| !s.contains("```")),
            "no fence markers expected: {texts:?}"
        );
        // Expect the heading and no fence markers. A blank separator may or may not be rendered at start.
        assert!(
            texts.iter().any(|s| s == "## Heading"),
            "expected heading line: {texts:?}"
        );
    }

    #[tokio::test]
    async fn paragraph_then_empty_fence_then_heading_keeps_heading_on_new_line() {
        let deltas = vec!["Para.\n", "```\n```\n", "## Title\n"]; // empty fence block in one commit
        let streamed = simulate_stream_markdown_for_tests(&deltas, true);
        let texts = lines_to_plain_strings(&streamed);
        let para_idx = match texts.iter().position(|s| s == "Para.") {
            Some(i) => i,
            None => panic!("para present"),
        };
        let head_idx = match texts.iter().position(|s| s == "## Title") {
            Some(i) => i,
            None => panic!("heading present"),
        };
        assert!(
            head_idx > para_idx,
            "heading should not merge with paragraph: {texts:?}"
        );
    }

    #[tokio::test]
    async fn loose_list_with_split_dashes_matches_full_render() {
        // Minimized failing sequence discovered by the helper: two chunks
        // that still reproduce the mismatch.
        let deltas = vec!["- item.\n\n", "-"];

        let streamed = simulate_stream_markdown_for_tests(&deltas, true);
        let streamed_strs = lines_to_plain_strings(&streamed);

        let full: String = deltas.iter().copied().collect();
        let mut rendered_all: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown(&full, None, &mut rendered_all);
        let rendered_all_strs = lines_to_plain_strings(&rendered_all);

        assert_eq!(
            streamed_strs, rendered_all_strs,
            "streamed output should match full render without dangling '-' lines"
        );
    }

    #[tokio::test]
    async fn loose_vs_tight_list_items_streaming_matches_full() {
        // Deltas extracted from the session log around 2025-08-27T00:33:18.216Z
        let deltas = vec![
            "\n\n",
            "Loose",
            " vs",
            ".",
            " tight",
            " list",
            " items",
            ":\n",
            "1",
            ".",
            " Tight",
            " item",
            "\n",
            "2",
            ".",
            " Another",
            " tight",
            " item",
            "\n\n",
            "1",
            ".",
            " Loose",
            " item",
            " with",
            " its",
            " own",
            " paragraph",
            ".\n\n",
            "  ",
            " This",
            " paragraph",
            " belongs",
            " to",
            " the",
            " same",
            " list",
            " item",
            ".\n\n",
            "2",
            ".",
            " Second",
            " loose",
            " item",
            " with",
            " a",
            " nested",
            " list",
            " after",
            " a",
            " blank",
            " line",
            ".\n\n",
            "  ",
            " -",
            " Nested",
            " bullet",
            " under",
            " a",
            " loose",
            " item",
            "\n",
            "  ",
            " -",
            " Another",
            " nested",
            " bullet",
            "\n\n",
        ];

        let streamed = simulate_stream_markdown_for_tests(&deltas, true);
        let streamed_strs = lines_to_plain_strings(&streamed);

        // Compute a full render for diagnostics only.
        let full: String = deltas.iter().copied().collect();
        let mut rendered_all: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown(&full, None, &mut rendered_all);

        // Also assert exact expected plain strings for clarity.
        let expected = vec![
            "Loose vs. tight list items:".to_string(),
            "".to_string(),
            "1. Tight item".to_string(),
            "2. Another tight item".to_string(),
            "3. Loose item with its own paragraph.".to_string(),
            "".to_string(),
            "   This paragraph belongs to the same list item.".to_string(),
            "4. Second loose item with a nested list after a blank line.".to_string(),
            "    - Nested bullet under a loose item".to_string(),
            "    - Another nested bullet".to_string(),
        ];
        assert_eq!(
            streamed_strs, expected,
            "expected exact rendered lines for loose/tight section"
        );
    }

    // Targeted tests derived from fuzz findings. Each asserts streamed == full render.
    async fn assert_streamed_equals_full(deltas: &[&str]) {
        let streamed = simulate_stream_markdown_for_tests(deltas, true);
        let streamed_strs = lines_to_plain_strings(&streamed);
        let full: String = deltas.iter().copied().collect();
        let mut rendered: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown(&full, None, &mut rendered);
        let rendered_strs = lines_to_plain_strings(&rendered);
        assert_eq!(streamed_strs, rendered_strs, "full:\n---\n{full}\n---");
    }

    #[tokio::test]
    async fn fuzz_class_bullet_duplication_variant_1() {
        assert_streamed_equals_full(&[
            "aph.\n- let one\n- bull",
            "et two\n\n  second paragraph \n",
        ])
        .await;
    }

    #[tokio::test]
    async fn fuzz_class_bullet_duplication_variant_2() {
        assert_streamed_equals_full(&[
            "- e\n  c",
            "e\n- bullet two\n\n  second paragraph in bullet two\n",
        ])
        .await;
    }

    #[tokio::test]
    async fn streaming_html_block_then_text_matches_full() {
        assert_streamed_equals_full(&[
            "HTML block:\n",
            "<div>inline block</div>\n",
            "more stuff\n",
        ])
        .await;
    }

    /// Stream `source` one character at a time and return the committed lines.
    ///
    /// Character-at-a-time is the worst case for a table: every row arrives in its own commit, so
    /// any line committed before the table closes is a line the final table would have rewritten.
    fn stream_char_by_char(source: &str, width: Option<usize>) -> Vec<Line<'static>> {
        let mut collector = MarkdownStreamCollector::new(width);
        let mut out = Vec::new();
        for character in source.chars() {
            let mut delta = [0u8; 4];
            let delta = character.encode_utf8(&mut delta);
            collector.push_delta(delta);
            if character == '\n' {
                out.extend(collector.commit_complete_lines());
            }
        }
        out.extend(collector.finalize_and_drain().0);
        out
    }

    fn assert_streamed_table_equals_full(source: &str, width: Option<usize>) {
        let streamed = lines_to_plain_strings(&stream_char_by_char(source, width));
        let mut rendered: Vec<Line<'static>> = Vec::new();
        crate::markdown::append_markdown(source, width, &mut rendered);
        assert_eq!(
            streamed,
            lines_to_plain_strings(&rendered),
            "streamed table should match the full render at width {width:?}"
        );
    }

    const STREAMED_TABLE: &str = concat!(
        "Here is a table:\n",
        "\n",
        "| Situation | Commit strategy |\n",
        "| --- | --- |\n",
        "| Streaming/tail append | Existing incremental insertion |\n",
        "| Width changes | Debounced full replay |\n",
        "| Height-only changes | Replay only when viewport bookkeeping requires it |\n",
        "| Rollback/session replacement/resume | Full replay |\n",
        "| Change above writable viewport | Full replay |\n",
    );

    #[tokio::test]
    async fn streamed_table_matches_full_render() {
        for width in [Some(60), Some(80), Some(120), None] {
            assert_streamed_table_equals_full(STREAMED_TABLE, width);
        }
    }

    #[tokio::test]
    async fn streamed_table_followed_by_prose_matches_full() {
        let source = format!("{STREAMED_TABLE}\nAnd some trailing prose.\n");
        assert_streamed_table_equals_full(&source, Some(80));
    }

    #[tokio::test]
    async fn streamed_table_never_commits_raw_pipe_syntax() {
        let streamed = lines_to_plain_strings(&stream_char_by_char(STREAMED_TABLE, Some(80)));
        assert!(
            !streamed.iter().any(|line| line.contains('|')),
            "raw markdown pipe syntax leaked into the transcript: {streamed:?}"
        );
        assert!(
            streamed.iter().any(|line| line.contains('━')),
            "expected a grid header rule in the transcript: {streamed:?}"
        );
    }

    #[tokio::test]
    async fn streamed_table_snapshot() {
        let transcript = lines_to_plain_strings(&stream_char_by_char(STREAMED_TABLE, Some(80)))
            .into_iter()
            .map(|line| line.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!("streamed_markdown_table", transcript);
    }

    #[tokio::test]
    async fn streamed_table_commits_column_rules_of_one_width() {
        let streamed = lines_to_plain_strings(&stream_char_by_char(STREAMED_TABLE, Some(80)));
        let rules = streamed
            .iter()
            .filter(|line| line.contains('─'))
            .map(|line| line.trim_end().to_string())
            .collect::<Vec<_>>();
        let distinct = rules.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            distinct.len(),
            1,
            "column widths drifted mid-table across commits: {rules:?}"
        );
    }
}
