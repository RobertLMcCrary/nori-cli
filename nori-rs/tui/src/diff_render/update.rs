//! An edit's two texts diffed with disparate, and drawn as unified hunks.
//!
//! In a language disparate has a grammar for, the texts are diffed structurally
//! ([`Structural`]): code that was only re-indented is unchanged, code that was only rewrapped
//! has nothing new in it, and code that moved is not new where it went. The structural diff's
//! lines place the hunks, and its novel tokens are what is emphasised on any changed line.
//! Otherwise lines are diffed, and a changed line paired with the line it is an edit of
//! ([`View::rows`](disparate::View::rows)) has the words that differ emphasised. A line that is
//! new throughout is only tinted, as an added line is.
//!
//! Either way the cost is that of the changes: equal lines are compared as bytes and never
//! split or highlighted, only the windows around the changes are parsed, and only the lines
//! shown are highlighted.

use core::range::Range;
use std::path::Path;

use disparate::Both;
use disparate::Diff;
use disparate::Differ;
use disparate::Part;
use disparate::Refiner;
use disparate::Side;
use disparate::Text;
use disparate::Words;
use disparate::syntax::Language;
use disparate::syntax::Structural;
use ratatui::style::Stylize;
use ratatui::text::Line as RtLine;
use ratatui::text::Span as RtSpan;

use super::highlighted_diff_lines_for_path;
use super::line::DiffLineLayout;
use super::line::DiffLineType;
use super::line::content_spans;
use super::line::diff_line_rows;
use super::line::line_number_width;
use super::style::DiffRenderStyleContext;
use super::style::style_gutter;

/// Unchanged lines shown either side of a hunk; the rest fold away, `⋮` between the hunks.
const CONTEXT_LINES: usize = 3;

/// The two texts of an edit and their diff: the lines that changed and, for a structural
/// diff, the novel bytes of each side.
pub(super) struct EditDiff<'a> {
    sources: Both<&'a str>,
    texts: Both<Text<&'a [u8]>>,
    lines: Diff,
    /// The novel byte ranges of each side, sorted and disjoint, when the texts were diffed by
    /// their grammar.
    novel: Option<Both<Vec<Range<usize>>>>,
}

/// A line of an edit as it is shown.
struct Shown {
    kind: DiffLineType,
    /// Its index in its own side's text: the old text for a removed line, else the new.
    line: usize,
    /// The byte ranges of the line, from its start, to emphasise.
    emphasis: Vec<Range<usize>>,
}

impl<'a> EditDiff<'a> {
    /// The diff of `old_text` and `new_text`, read through the grammar `path` names when
    /// disparate has it and both texts parse there.
    pub(super) fn new(old_text: &'a str, new_text: &'a str, path: &Path) -> Self {
        let sources = Both::new(old_text, new_text);
        let bytes = sources.map(str::as_bytes);
        let texts = bytes.map(Text::lines);
        let lens = texts.as_ref().map(Text::len);
        // Code that does not parse is diffed as lines: tree-sitter's error recovery is not
        // the code.
        let structural = Language::from_path(path)
            .and_then(|language| Structural::parse(language, bytes.before, bytes.after))
            .filter(|diff| !diff.has_errors())
            .map(|diff| {
                let novel = Both::new(
                    diff.novel(Side::Before).collect(),
                    diff.novel(Side::After).collect(),
                );
                (diff.lines(), novel)
            })
            // Reading a diff through texts of other lengths panics; never risk it on the UI
            // thread.
            .filter(|(lines, _)| lines.len() == lens);
        let (lines, novel) = match structural {
            Some((lines, novel)) => (lines, Some(novel)),
            None => (Differ::new().diff_lines(bytes.before, bytes.after), None),
        };
        Self {
            sources,
            texts,
            lines,
            novel,
        }
    }

    /// How many lines were removed and added.
    pub(super) fn changed(&self) -> Both<usize> {
        self.lines.changed()
    }

    /// Draws the hunks into `out` within `width`, numbered from `first_line` and highlighted
    /// as `path`'s language.
    pub(super) fn render(
        &self,
        first_line: usize,
        path: &Path,
        width: usize,
        ctx: &DiffRenderStyleContext,
        out: &mut Vec<RtLine<'static>>,
    ) {
        let groups = self.shown();
        let side_of = |kind: DiffLineType| match kind {
            DiffLineType::Delete => Side::Before,
            DiffLineType::Insert | DiffLineType::Context => Side::After,
        };
        let max_line_number = groups
            .iter()
            .flat_map(|(_, shown)| shown)
            .map(|shown| shown.line + first_line)
            .max()
            .unwrap_or(0);
        let layout = DiffLineLayout {
            width,
            line_number_width: line_number_width(max_line_number),
        };
        for (index, (window, shown)) in groups.iter().enumerate() {
            if index > 0 {
                let spacer = format!("{:width$} ", "", width = layout.line_number_width.max(1));
                out.push(RtLine::from(vec![
                    RtSpan::styled(spacer, style_gutter()),
                    "⋮".dim(),
                ]));
            }
            // Each side's lines in the window are highlighted together, in their own text,
            // and only when some of them are shown.
            let syntax = Both::new(Side::Before, Side::After).map(|side| {
                let used = shown.iter().any(|line| side_of(line.kind) == side);
                let span = self.texts[side].span(window[side]);
                used.then(|| &self.sources[side][span.start..span.end])
                    .and_then(|lines| highlighted_diff_lines_for_path(lines, path))
            });
            for line in shown {
                let side = side_of(line.kind);
                let span = self.texts[side].span(Range::from(line.line..line.line + 1));
                let text = self.sources[side][span.start..span.end].trim_end_matches(['\n', '\r']);
                let spans = syntax[side]
                    .as_ref()
                    .and_then(|lines| lines.get(line.line - window[side].start))
                    .map(Vec::as_slice);
                let content = content_spans(line.kind, text, spans, &line.emphasis, ctx);
                out.extend(diff_line_rows(
                    line.line + first_line,
                    line.kind,
                    content,
                    layout,
                    ctx,
                ));
            }
        }
    }

    /// The lines each hunk shows, with the window of lines it spans: unchanged lines as
    /// context, its removed lines before its added ones, as a unified patch has them.
    fn shown(&self) -> Vec<(Both<Range<usize>>, Vec<Shown>)> {
        let view = self.lines.on(&self.texts.before, &self.texts.after);
        let mut words = Refiner::new(Words);
        view.groups(CONTEXT_LINES)
            .map(|group| {
                let mut shown = Vec::new();
                // Added lines wait for the end of their hunk, below the removed ones.
                let mut added = Vec::new();
                for row in group.rows() {
                    let lines = row.tokens();
                    if !row.is_changed() {
                        shown.append(&mut added);
                        shown.push(Shown {
                            kind: DiffLineType::Context,
                            line: lines.after.start,
                            emphasis: Vec::new(),
                        });
                        continue;
                    }
                    let mut emphasis = self.emphasis(&row, &mut words);
                    if !lines.before.is_empty() {
                        shown.push(Shown {
                            kind: DiffLineType::Delete,
                            line: lines.before.start,
                            emphasis: std::mem::take(&mut emphasis.before),
                        });
                    }
                    if !lines.after.is_empty() {
                        added.push(Shown {
                            kind: DiffLineType::Insert,
                            line: lines.after.start,
                            emphasis: emphasis.after,
                        });
                    }
                }
                shown.append(&mut added);
                (group.window(), shown)
            })
            .collect()
    }

    /// What changed in each line of a changed row, as byte ranges from the start of the line,
    /// blanks at their ends left out: the novel tokens on it when the diff is structural, else,
    /// when the row pairs a line with the line it is an edit of, the words that differ between
    /// them. A line that is new throughout is left to its tint, as a line that was only added.
    fn emphasis(&self, row: &Part<'_>, words: &mut Refiner<Words>) -> Both<Vec<Range<usize>>> {
        let (lines, span) = (row.tokens(), row.span());
        let changed: Both<Vec<Range<usize>>> = match &self.novel {
            Some(novel) => novel.as_ref().zip(span).map(|(novel, line)| {
                let from = novel.partition_point(|r| r.end <= line.start);
                novel[from..]
                    .iter()
                    .take_while(|r| r.start < line.end)
                    .map(|r| Range::from(r.start.max(line.start)..r.end.min(line.end)))
                    .collect()
            }),
            None if !lines.before.is_empty() && !lines.after.is_empty() => {
                let refined = words.refine(row);
                Both::new(
                    refined.changed(Side::Before).collect(),
                    refined.changed(Side::After).collect(),
                )
            }
            None => Both::default(),
        };
        changed
            .zip(span)
            .zip(self.texts.as_ref())
            .map(|((ranges, line), text)| {
                let bytes = &text.bytes()[line];
                // A novel run joined across lines takes in the line break and indent between.
                let trim = |mut start: usize, mut end: usize| {
                    while start < end && bytes[start].is_ascii_whitespace() {
                        start += 1;
                    }
                    while end > start && bytes[end - 1].is_ascii_whitespace() {
                        end -= 1;
                    }
                    (start < end).then(|| Range::from(start..end))
                };
                let ranges: Vec<Range<usize>> = ranges
                    .into_iter()
                    .filter_map(|r| trim(r.start - line.start, r.end - line.start))
                    .collect();
                if ranges.len() == 1 && ranges.first().copied() == trim(0, bytes.len()) {
                    Vec::new()
                } else {
                    ranges
                }
            })
    }
}
