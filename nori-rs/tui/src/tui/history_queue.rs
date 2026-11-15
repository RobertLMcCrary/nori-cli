//! History waiting to be written above the inline viewport, and where each frame puts the
//! viewport.
//!
//! A frame places the viewport for the height it is drawn at, then writes the history that was
//! queued since the previous frame above it. A repaint comes first in the same step: the history
//! rows Nori owns are written again where they are, from the record the terminal keeps of what it
//! wrote there (`WrittenHistory`). The repaint never reads or replaces the queue, so a line
//! committed in the same frame as a repaint is written exactly once, whichever came first.

use std::io;

use ratatui::backend::Backend;
use ratatui::text::Line;

use crate::custom_terminal::Terminal;

/// History lines queued for the next frame to write above the viewport.
#[derive(Debug, Default)]
pub(crate) struct HistoryQueue {
    lines: Vec<Line<'static>>,
    /// The next frame writes the history rows Nori owns again, and all of the viewport.
    repaint: bool,
}

impl HistoryQueue {
    pub(crate) fn push(&mut self, lines: Vec<Line<'static>>) {
        self.lines.extend(lines);
    }

    pub(crate) fn clear(&mut self) {
        self.lines.clear();
    }

    pub(crate) fn request_repaint(&mut self) {
        self.repaint = true;
    }

    /// Repaint what Nori owns if a repaint was asked for, place the viewport for a frame `height`
    /// rows tall, and write the queued history above it, up to a frame's worth. Returns whether
    /// history is left for the next frame to write.
    pub(crate) fn write_frame<B>(
        &mut self,
        terminal: &mut Terminal<B>,
        height: u16,
    ) -> io::Result<bool>
    where
        B: Backend<Error = io::Error> + io::Write,
    {
        // Repaint where the rows are now, before placing the viewport can scroll any of them
        // into the scrollback, which nothing can repaint.
        if std::mem::take(&mut self.repaint) {
            crate::insert_history::repaint_owned_history(terminal)?;
            terminal.clear()?;
        }

        let size = terminal.size()?;
        let mut area = terminal.viewport_area;
        area.height = height.min(size.height);
        area.width = size.width;
        // If the viewport has expanded, scroll everything else up to make room.
        if area.bottom() > size.height {
            terminal
                .backend_mut()
                .scroll_region_up(0..area.top(), area.bottom() - size.height)?;
            area.y = size.height - area.height;
        } else if area.y == 0 && area.height < size.height {
            // Viewport was full-screen (y=0) and has shrunk. Reposition
            // to bottom so insert_history_lines has room above.
            let new_y = size.height - area.height;
            area.y = new_y;

            // Set viewport area and clear BEFORE writing history into
            // the vacated rows. clear() erases from viewport_area.y
            // downward, so we must reposition first to avoid wiping
            // content we're about to paint above the viewport.
            terminal.set_viewport_area(area);
            terminal.clear()?;
            // The rows above the viewport were the viewport's: none of them is history.
            terminal.written_history.clear();

            // Write pending history lines directly into the vacated rows
            // to avoid pushing stale viewport content into scrollback.
            if !self.lines.is_empty() {
                crate::insert_history::write_pending_lines_directly(
                    terminal,
                    &mut self.lines,
                    new_y,
                )?;
            } else {
                // No pending lines — just clear the stale rows.
                let writer = terminal.backend_mut();
                for row in 0..new_y {
                    crossterm::queue!(
                        writer,
                        crossterm::cursor::MoveTo(0, row),
                        crossterm::terminal::Clear(crossterm::terminal::ClearType::UntilNewLine)
                    )?;
                }
            }
        }
        if area != terminal.viewport_area {
            // TODO(nornagon): probably this could be collapsed with the clear + set_viewport_area above.
            terminal.clear()?;
            terminal.set_viewport_area(area);
        }

        if self.lines.is_empty() {
            return Ok(false);
        }
        // A frame writes a bounded batch from the front of the queue; the rest waits for the next.
        let mut bytes = 0;
        let batch_len = self
            .lines
            .iter()
            .take_while(|line| {
                let fits = bytes < MAX_HISTORY_BYTES_PER_FRAME;
                bytes += line
                    .spans
                    .iter()
                    .map(|span| span.content.len())
                    .sum::<usize>()
                    + STYLE_BYTES_PER_SPAN * line.spans.len();
                fits
            })
            .count()
            .min(MAX_HISTORY_LINES_PER_FRAME);
        let inserted =
            crate::insert_history::insert_history_lines(terminal, &self.lines[..batch_len])?;
        // History that cannot be written yet (no room above the viewport) stays queued, in order.
        if inserted {
            self.lines.drain(..batch_len);
        }
        Ok(inserted && !self.lines.is_empty())
    }
}

/// The most history lines one frame writes, and about the most bytes. A burst bigger than that
/// goes out over consecutive frames: its first rows show at once instead of after the whole of it
/// was written, and no synchronized update grows so large that a terminal that stops waiting for
/// its end after a short timeout draws it half done.
const MAX_HISTORY_LINES_PER_FRAME: usize = 2_000;
const MAX_HISTORY_BYTES_PER_FRAME: usize = 256 * 1024;
/// What the escape sequences that colour a span cost, about, on top of its text.
const STYLE_BYTES_PER_SPAN: usize = 24;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_backend::VT100Backend;
    use pretty_assertions::assert_eq;
    use ratatui::layout::Rect;
    use ratatui::style::Stylize;

    #[test]
    fn a_burst_is_written_over_bounded_frames_each_line_once_in_order() {
        let mut terminal =
            Terminal::with_options(VT100Backend::with_scrollback(60, 20)).expect("terminal");
        terminal.set_viewport_area(Rect::new(0, 18, 60, 2));
        let mut queue = HistoryQueue::default();
        queue.push(
            (0..5_000)
                .map(|index| {
                    Line::from(vec![
                        format!("{index:05} ").cyan(),
                        "let value = compute(x, y);".into(),
                        " // comment".dim(),
                    ])
                })
                .collect(),
        );

        let mut frame_bytes = Vec::new();
        loop {
            terminal.backend_mut().start_recording();
            let more = queue.write_frame(&mut terminal, 2).expect("frame");
            terminal.draw(|_| {}).expect("draw");
            frame_bytes.push(terminal.backend_mut().take_recording().len());
            if !more {
                break;
            }
        }

        assert!(frame_bytes.len() > 1, "{frame_bytes:?}");
        assert!(
            frame_bytes
                .iter()
                .all(|bytes| *bytes <= MAX_HISTORY_BYTES_PER_FRAME + 4 * 1024),
            "{frame_bytes:?}"
        );
        let screen = terminal.backend().vt100().screen();
        let written: Vec<String> = terminal
            .backend()
            .scrollback_rows()
            .iter()
            .map(|(text, _)| text.clone())
            .chain(screen.rows(0, 60).take(18))
            // The rows that were empty before the first line was written scrolled away first.
            .filter(|row| !row.is_empty())
            .collect();
        let expected: Vec<String> = (0..5_000)
            .map(|index| format!("{index:05} let value = compute(x, y); // comment"))
            .collect();
        assert_eq!(written, expected);
    }
}
