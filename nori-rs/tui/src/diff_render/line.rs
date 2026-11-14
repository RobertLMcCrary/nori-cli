//! One line of a diff as it is drawn: its content styled (syntax colours, the line's tint, the
//! changed words emphasised), then the right-aligned line number and sign in front, wrapped to
//! the width.

use core::range::Range;

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line as RtLine;
use ratatui::text::Span as RtSpan;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use super::style::DiffRenderStyleContext;
use super::style::style_add;
use super::style::style_context;
use super::style::style_del;
use super::style::style_gutter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiffLineType {
    Insert,
    Delete,
    Context,
}

#[derive(Clone, Copy)]
pub(super) struct DiffLineLayout {
    pub(super) width: usize,
    pub(super) line_number_width: usize,
}

/// The content of one diff line styled for drawing: the highlighter's colours where `syntax`
/// has them (dimmed on a removed line) and the line's own colour elsewhere, on the line's tint,
/// except that the `emphasis` byte ranges of `text` (sorted, disjoint) are drawn with the
/// stronger emphasis instead, keeping their syntax colours.
pub(super) fn content_spans(
    kind: DiffLineType,
    text: &str,
    syntax: Option<&[RtSpan<'static>]>,
    emphasis: &[Range<usize>],
    ctx: &DiffRenderStyleContext,
) -> Vec<RtSpan<'static>> {
    let (plain, tint, emphasized) = match kind {
        DiffLineType::Insert => (style_add(ctx), ctx.add_bg, ctx.add_emphasis),
        DiffLineType::Delete => (style_del(ctx), ctx.del_bg, ctx.del_emphasis),
        DiffLineType::Context => (style_context(), None, Style::default()),
    };
    // The highlighter's spans cover the line in order: as classes, they are its byte ranges.
    let mut at = 0;
    let classes: Vec<(Range<usize>, Style)> = syntax
        .unwrap_or_default()
        .iter()
        .map(|span| {
            let start = at;
            at += span.content.len();
            (Range::from(start..at), span.style)
        })
        .collect();
    disparate::layout::segments(Range::from(0..text.len()), &classes, emphasis)
        .map(|segment| {
            let style = match segment.class {
                // A removed line's syntax colours are dimmed, but for the words that changed.
                Some(class) if kind == DiffLineType::Delete && !segment.changed => {
                    class.add_modifier(Modifier::DIM)
                }
                Some(class) => class,
                None => plain,
            };
            let style = if segment.changed {
                style.patch(emphasized)
            } else {
                tint.map_or(style, |bg| style.bg(bg))
            };
            let bytes = &text.as_bytes()[segment.range];
            RtSpan::styled(String::from_utf8_lossy(bytes).into_owned(), style)
        })
        .collect()
}

/// One diff line as the rows it wraps to: the line number right-aligned in the gutter and the
/// sign on the first row, the styled `content` wrapped under the sign within `layout.width`,
/// and on a tinted line the tint carried through the gutter to the right edge.
pub(super) fn diff_line_rows(
    line_number: usize,
    kind: DiffLineType,
    content: Vec<RtSpan<'static>>,
    layout: DiffLineLayout,
    ctx: &DiffRenderStyleContext,
) -> Vec<RtLine<'static>> {
    let ln_str = line_number.to_string();

    // Reserve a fixed number of spaces (equal to the widest line number plus a
    // trailing spacer) so the sign column stays aligned across the diff block.
    let gutter_width = layout.line_number_width.max(1);
    let prefix_cols = gutter_width + 1;

    let mut first = true;
    let (sign_char, line_style) = match kind {
        DiffLineType::Insert => ('+', style_add(ctx)),
        DiffLineType::Delete => ('-', style_del(ctx)),
        DiffLineType::Context => (' ', style_context()),
    };

    // Build a line-level background style so the bg extends edge-to-edge.
    let line_bg_style = match kind {
        DiffLineType::Insert => ctx.add_bg.map(|bg| Style::default().bg(bg)),
        DiffLineType::Delete => ctx.del_bg.map(|bg| Style::default().bg(bg)),
        DiffLineType::Context => None,
    };

    let mut lines: Vec<RtLine<'static>> = Vec::new();
    let available_content_cols = layout.width.saturating_sub(prefix_cols + 1).max(1);
    let wrapped_chunks = wrap_styled_spans(&content, available_content_cols);

    for chunk in wrapped_chunks {
        let (gutter_span, sign_span, used_cols) = if first {
            let gutter = format!("{ln_str:>gutter_width$} ");
            let sign = sign_char.to_string();
            let cols = UnicodeWidthStr::width(gutter.as_str())
                + UnicodeWidthStr::width(sign.as_str())
                + spans_display_width(&chunk);
            first = false;
            (gutter, Some(sign), cols)
        } else {
            let gutter = format!("{:gutter_width$}  ", "");
            let cols = UnicodeWidthStr::width(gutter.as_str()) + spans_display_width(&chunk);
            (gutter, None, cols)
        };

        // When a background tint is active, apply it to the gutter and sign (the content
        // carries its own) and pad with trailing spaces so the color fills the complete
        // width; the line's style carries it through any indent put in front of it.
        let line = if let Some(bg_style) = line_bg_style {
            let gutter_style = style_gutter().patch(bg_style);
            let mut spans = vec![RtSpan::styled(gutter_span, gutter_style)];
            if let Some(sign) = sign_span {
                spans.push(RtSpan::styled(sign, line_style.patch(bg_style)));
            }
            spans.extend(chunk);
            let pad = layout.width.saturating_sub(used_cols);
            if pad > 0 {
                spans.push(RtSpan::styled(" ".repeat(pad), bg_style));
            }
            RtLine::from(spans).style(bg_style)
        } else {
            let mut spans = vec![RtSpan::styled(gutter_span, style_gutter())];
            if let Some(sign) = sign_span {
                spans.push(RtSpan::styled(sign, line_style));
            }
            spans.extend(chunk);
            RtLine::from(spans)
        };

        lines.push(line);
    }
    lines
}

fn wrap_styled_spans(spans: &[RtSpan<'static>], max_cols: usize) -> Vec<Vec<RtSpan<'static>>> {
    let max_cols = max_cols.max(1);
    let mut rows: Vec<Vec<RtSpan<'static>>> = Vec::new();
    let mut current: Vec<RtSpan<'static>> = Vec::new();
    let mut current_cols = 0;

    for span in spans {
        let style = span.style;
        let mut buf = String::new();
        let mut buf_cols = 0;

        for ch in span.content.chars() {
            let ch_cols = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_cols + buf_cols + ch_cols > max_cols
                && (!current.is_empty() || !buf.is_empty())
            {
                if !buf.is_empty() {
                    current.push(RtSpan::styled(std::mem::take(&mut buf), style));
                    buf_cols = 0;
                }
                rows.push(std::mem::take(&mut current));
                current_cols = 0;
            }

            buf.push(ch);
            buf_cols += ch_cols;

            if current_cols + buf_cols == max_cols {
                current.push(RtSpan::styled(std::mem::take(&mut buf), style));
                rows.push(std::mem::take(&mut current));
                current_cols = 0;
                buf_cols = 0;
            }
        }

        if !buf.is_empty() {
            current_cols += buf_cols;
            current.push(RtSpan::styled(buf, style));
        }
    }

    if !current.is_empty() {
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(vec![RtSpan::from(String::new())]);
    }
    rows
}

fn spans_display_width(spans: &[RtSpan<'static>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

pub(super) fn line_number_width(max_line_number: usize) -> usize {
    if max_line_number == 0 {
        1
    } else {
        max_line_number.to_string().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_render::highlighted_diff_lines_for_path;
    use crate::diff_render::style::DiffColorLevel;
    use crate::diff_render::style::DiffTheme;
    use crate::render::line_utils::prefix_lines;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use ratatui::text::Text;
    use ratatui::widgets::Paragraph;
    use ratatui::widgets::Widget;
    use ratatui::widgets::Wrap;
    use std::path::Path;

    fn push_wrapped_diff_line(
        line_number: usize,
        kind: DiffLineType,
        text: &str,
        layout: DiffLineLayout,
        ctx: &DiffRenderStyleContext,
    ) -> Vec<RtLine<'static>> {
        let content = content_spans(kind, text, None, &[], ctx);
        diff_line_rows(line_number, kind, content, layout, ctx)
    }

    fn truecolor() -> DiffRenderStyleContext {
        DiffRenderStyleContext::new(DiffTheme::Dark, DiffColorLevel::TrueColor)
    }

    #[test]
    fn ui_snapshot_wrap_behavior_insert() {
        // Narrow width to force wrapping within our diff line rendering
        let long_line = "this is a very long line that should wrap across multiple terminal columns and continue";
        let ctx = DiffRenderStyleContext::new(DiffTheme::Dark, DiffColorLevel::Ansi16);

        // Call the wrapping function directly so we can precisely control the width
        let lines = push_wrapped_diff_line(
            1,
            DiffLineType::Insert,
            long_line,
            DiffLineLayout {
                width: 80,
                line_number_width: line_number_width(1),
            },
            &ctx,
        );

        // Render into a small terminal to capture the visual layout
        let mut terminal = Terminal::new(TestBackend::new(90, 8)).expect("terminal");
        terminal
            .draw(|f| {
                Paragraph::new(Text::from(lines))
                    .wrap(Wrap { trim: false })
                    .render(f.area(), f.buffer_mut())
            })
            .expect("draw");
        assert_snapshot!("wrap_behavior_insert", terminal.backend());
    }

    #[test]
    fn diff_bg_extends_through_prefix_lines_indent() {
        // Verifies that push_wrapped_diff_line produces lines where
        // prefix_lines can propagate the bg to the indent prefix,
        // giving edge-to-edge background highlighting.
        #[allow(clippy::disallowed_methods)]
        let green_bg = Color::Rgb(33, 58, 43);
        let total_width: u16 = 40;
        let prefix_width: usize = 4;
        let area = Rect::new(0, 0, total_width, 4);
        let ctx = truecolor();

        // Produce a diff line the same way production code does
        let diff_lines = push_wrapped_diff_line(
            1,
            DiffLineType::Insert,
            "hello",
            DiffLineLayout {
                width: (total_width as usize) - prefix_width,
                line_number_width: 1,
            },
            &ctx,
        );

        // Apply prefix_lines just like render_changes_block does
        let lines = prefix_lines(diff_lines, "    ".into(), "    ".into());

        let mut buf = Buffer::empty(area);
        Paragraph::new(Text::from(lines)).render(area, &mut buf);

        // Every cell in the row should have the green background
        for col in 0..total_width {
            assert_eq!(
                buf[(col, 0)].bg,
                green_bg,
                "col {col} should have bg — edge-to-edge fill requires all cells covered"
            );
        }
    }

    #[test]
    fn diff_bg_fills_full_width() {
        // With a truecolor context, each diff line's spans should cover the
        // full requested width so the background tint extends edge-to-edge.
        let ctx = truecolor();
        let total_width: usize = 36;
        let lines = push_wrapped_diff_line(
            1,
            DiffLineType::Insert,
            "hello",
            DiffLineLayout {
                width: total_width,
                line_number_width: 1,
            },
            &ctx,
        );
        assert_eq!(lines.len(), 1);
        let total_chars: usize = lines[0].spans.iter().map(|s| s.content.len()).sum();
        assert_eq!(
            total_chars, total_width,
            "expected line to fill {total_width} columns, got {total_chars}"
        );
        // The line-level style should also carry the background
        assert!(
            lines[0].style.bg.is_some(),
            "expected line-level background style"
        );
        // Gutter span (first) should also have the background
        assert!(
            lines[0].spans[0].style.bg.is_some(),
            "expected gutter span to have background"
        );
    }

    #[test]
    fn diff_insert_content_uses_syntax_highlighting_when_path_is_known() {
        let ctx = truecolor();
        let text = "fn main() { println!(\"hello\"); }";
        let syntax = highlighted_diff_lines_for_path(text, Path::new("src/main.rs"))
            .and_then(|mut lines| (lines.len() == 1).then(|| lines.remove(0)));
        let content = content_spans(DiffLineType::Insert, text, syntax.as_deref(), &[], &ctx);
        let lines = diff_line_rows(
            1,
            DiffLineType::Insert,
            content,
            DiffLineLayout {
                width: 80,
                line_number_width: 1,
            },
            &ctx,
        );

        assert!(
            lines[0].spans.iter().skip(2).any(|span| {
                span.style.fg.is_some()
                    && span.style.fg != Some(Color::Green)
                    && span.style.bg == ctx.add_bg
            }),
            "expected inserted Rust content to use syntax colors over the add background, got {:?}",
            lines[0].spans
        );
    }

    /// The emphasised words of a line take the stronger tint and keep the highlighter's colours;
    /// on a removed line they are not dimmed with the rest.
    #[test]
    fn emphasis_keeps_syntax_colours_on_the_stronger_tint() {
        let ctx = truecolor();
        let text = "let total = compute(a);";
        let syntax = highlighted_diff_lines_for_path(text, Path::new("lib.rs"))
            .and_then(|mut lines| (lines.len() == 1).then(|| lines.remove(0)));
        let emphasis = [Range::from(4..9)];
        let spans = content_spans(
            DiffLineType::Delete,
            text,
            syntax.as_deref(),
            &emphasis,
            &ctx,
        );
        let emphasised: Vec<&str> = spans
            .iter()
            .filter(|span| span.style.bg == ctx.del_emphasis.bg)
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(emphasised, ["total"]);
        let total = spans
            .iter()
            .find(|span| span.content == "total")
            .expect("the emphasised word is a span of its own");
        let highlighted = syntax
            .iter()
            .flatten()
            .find(|span| span.content.contains("total"))
            .expect("the highlighter's span over the word");
        assert_eq!(total.style.fg, highlighted.style.fg);
        assert!(!total.style.add_modifier.contains(Modifier::DIM));
        assert!(
            spans
                .iter()
                .filter(|span| span.content != "total")
                .all(|span| span.style.bg == ctx.del_bg
                    && span.style.add_modifier.contains(Modifier::DIM)),
            "the rest of the removed line keeps its tint and is dimmed: {spans:?}"
        );
    }

    /// Without backgrounds (16 colours), emphasis is bold and underlined.
    #[test]
    fn emphasis_without_backgrounds_is_bold_and_underlined() {
        let ctx = DiffRenderStyleContext::new(DiffTheme::Dark, DiffColorLevel::Ansi16);
        let emphasis = [Range::from(9..16)];
        let spans = content_spans(
            DiffLineType::Insert,
            "line two changed",
            None,
            &emphasis,
            &ctx,
        );
        let styled: Vec<(&str, Style)> = spans
            .iter()
            .map(|span| (span.content.as_ref(), span.style))
            .collect();
        let green = Style::default().fg(Color::Green);
        assert_eq!(
            styled,
            [
                ("line two ", green),
                (
                    "changed",
                    green.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                ),
            ]
        );
    }

    #[test]
    fn diff_wrapping_counts_unicode_display_width() {
        let ctx = DiffRenderStyleContext::new(DiffTheme::Dark, DiffColorLevel::Ansi16);
        let lines = push_wrapped_diff_line(
            1,
            DiffLineType::Insert,
            "界界界abc",
            DiffLineLayout {
                width: 8,
                line_number_width: 1,
            },
            &ctx,
        );

        assert!(
            lines.len() > 1,
            "wide content should wrap before exceeding the requested width"
        );
        for line in lines {
            let width: usize = line
                .spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum();
            assert!(
                width <= 8,
                "expected wrapped diff line width <= 8 columns, got {width}: {line:?}"
            );
        }
    }
}
