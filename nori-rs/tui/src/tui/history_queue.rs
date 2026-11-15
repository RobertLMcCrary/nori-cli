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
    /// The next frame scrolls everything above the prompt into the scrollback.
    clear_above: bool,
    /// The prompt has been placed at the bottom of the screen. Only the first frame and a clear
    /// do it: moving the screen down later would put blank rows into the middle of the
    /// scrollback each time the prompt shrinks.
    pinned: bool,
    /// Rows the viewport was stretched by for a popup that is open, drawn over what is on
    /// screen instead of scrolling it away: `up` over history rows above it, `down` over blank
    /// rows below it.
    stretch: Stretch,
}

/// How far a transient popup stretched the viewport past the prompt's own rows.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
struct Stretch {
    up: u16,
    down: u16,
}

impl HistoryQueue {
    pub(crate) fn push(&mut self, lines: Vec<Line<'static>>) {
        self.lines.extend(lines);
    }

    pub(crate) fn clear(&mut self) {
        self.lines.clear();
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Ctrl+L clears the screen above the prompt instead"
        )
    )]
    pub(crate) fn request_repaint(&mut self) {
        self.repaint = true;
    }

    /// Hide everything above the prompt: the next frame scrolls it into the scrollback, which is
    /// left whole, and leaves the screen above the prompt empty.
    pub(crate) fn request_clear_above(&mut self) {
        self.clear_above = true;
    }

    /// Repaint what Nori owns if a repaint was asked for, place the viewport for a frame `height`
    /// rows tall, and write the queued history above it, up to a frame's worth. Returns whether
    /// history is left for the next frame to write.
    #[cfg(test)]
    pub(crate) fn write_frame<B>(
        &mut self,
        terminal: &mut Terminal<B>,
        height: u16,
    ) -> io::Result<bool>
    where
        B: Backend<Error = io::Error> + io::Write,
    {
        self.write_frame_with_popup(terminal, height, 0)
    }

    /// [`Self::write_frame`] for a viewport whose last `popup_rows` rows belong to a popup. The
    /// popup is drawn over the screen above the prompt rather than scrolling it into the
    /// scrollback, so the prompt stays where it is and the rows it covered come back, from the
    /// record of what Nori wrote there, when the popup closes.
    pub(crate) fn write_frame_with_popup<B>(
        &mut self,
        terminal: &mut Terminal<B>,
        height: u16,
        popup_rows: u16,
    ) -> io::Result<bool>
    where
        B: Backend<Error = io::Error> + io::Write,
    {
        // Place the prompt on its own, as if no popup had ever been open.
        let stretched = terminal.viewport_area;
        let before = std::mem::take(&mut self.stretch);
        if before != Stretch::default() {
            let mut base = stretched;
            base.y += before.up;
            base.height -= before.up + before.down;
            // Only where the prompt is, as far as placing it goes: the buffers still hold what
            // the last frame drew, which a frame with the same popup compares itself with.
            terminal.viewport_area = base;
        }
        let base_before = terminal.viewport_area;
        let popup_rows = popup_rows.min(height.saturating_sub(1));
        let height = height - popup_rows;

        // Repaint where the rows are now, before placing the viewport can scroll any of them
        // into the scrollback, which nothing can repaint.
        if std::mem::take(&mut self.repaint) {
            crate::insert_history::repaint_owned_history(terminal)?;
            terminal.clear()?;
        }

        if std::mem::take(&mut self.clear_above) {
            let top = terminal.viewport_area.top();
            if top > 0 {
                terminal.backend_mut().scroll_region_up(0..top, top)?;
            }
            // What was above the prompt is in the scrollback now, where nothing repaints it.
            terminal.written_history.clear();
            terminal.clear()?;
            self.pinned = false;
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
        } else if area.bottom() < size.height && !self.pinned {
            // The prompt goes to the bottom of the screen: whatever is on screen moves down with
            // it, and only blank rows below the viewport fall off. The viewport is placed first,
            // since clearing erases from the viewport downward.
            let gap = size.height - area.bottom();
            terminal
                .backend_mut()
                .scroll_region_down(0..size.height, gap)?;
            area.y += gap;
            terminal.set_viewport_area(area);
            terminal.clear()?;
        }
        self.pinned = true;
        if area != terminal.viewport_area {
            // TODO(nornagon): probably this could be collapsed with the clear + set_viewport_area above.
            terminal.clear()?;
            terminal.set_viewport_area(area);
        }
        let stretch = self.stretch_for(terminal, popup_rows);
        self.show_popup(terminal, (before, stretched, base_before), stretch)?;

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

impl HistoryQueue {
    /// How far to stretch the viewport for `popup_rows` of popup: over the blank rows below it
    /// first, then over the history above it, always leaving two rows for history to scroll in.
    fn stretch_for<B>(&self, terminal: &Terminal<B>, popup_rows: u16) -> Stretch
    where
        B: Backend<Error = io::Error> + io::Write,
    {
        let area = terminal.viewport_area;
        let screen = terminal.size().map_or(area.bottom(), |size| size.height);
        let down = popup_rows.min(screen.saturating_sub(area.bottom()));
        let up = (popup_rows - down).min(area.top().saturating_sub(2));
        Stretch { up, down }
    }

    /// Stretch the viewport to `stretch`, having been `before` (when it was `stretched`). When a
    /// popup shrinks or closes, what it drew and covered is erased and the history it covered is
    /// written again above the prompt.
    fn show_popup<B>(
        &mut self,
        terminal: &mut Terminal<B>,
        (before, stretched, base_before): (Stretch, ratatui::layout::Rect, ratatui::layout::Rect),
        stretch: Stretch,
    ) -> io::Result<()>
    where
        B: Backend<Error = io::Error> + io::Write,
    {
        let base = terminal.viewport_area;
        let mut area = base;
        area.y -= stretch.up;
        area.height += stretch.up + stretch.down;
        if stretch == before && base == base_before {
            terminal.viewport_area = area;
            self.stretch = stretch;
            return Ok(());
        }
        let uncovered = before.up > stretch.up;
        if uncovered || (before.down > stretch.down) {
            // The popup left rows it had drawn on; wipe them, from where the last frame began.
            terminal.set_viewport_area(stretched);
            terminal.clear()?;
            terminal.set_viewport_area(base);
            if uncovered {
                crate::insert_history::repaint_owned_history(terminal)?;
            }
        }
        if area != terminal.viewport_area {
            if stretch == Stretch::default() {
                terminal.clear()?;
                terminal.set_viewport_area(area);
            } else {
                // The popup draws over what is there, so what is there goes first.
                terminal.set_viewport_area(area);
                terminal.clear()?;
            }
        }
        self.stretch = stretch;
        Ok(())
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
    use std::io::Write;

    const WIDTH: u16 = 30;
    const HEIGHT: u16 = 10;

    fn screen(terminal: &Terminal<VT100Backend>) -> Vec<String> {
        terminal.backend().vt100().screen().rows(0, WIDTH).collect()
    }

    fn terminal_after_a_shell() -> Terminal<VT100Backend> {
        let mut terminal =
            Terminal::with_options(VT100Backend::with_scrollback(WIDTH, HEIGHT)).expect("terminal");
        write!(
            terminal.backend_mut(),
            "\x1b[1;1H$ nori\x1b[2;1Hshell output"
        )
        .expect("shell rows");
        // The prompt starts where the cursor was, below the shell's rows.
        terminal.set_viewport_area(Rect::new(0, 2, WIDTH, 2));
        terminal
    }

    fn draw_prompt(terminal: &mut Terminal<VT100Backend>) {
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.buffer_mut().set_string(
                    area.x,
                    area.y,
                    "> prompt",
                    ratatui::style::Style::default(),
                );
                frame.buffer_mut().set_string(
                    area.x,
                    area.y + 1,
                    "footer",
                    ratatui::style::Style::default(),
                );
            })
            .expect("draw");
    }

    #[test]
    fn the_first_frame_puts_the_prompt_at_the_bottom_with_the_screen_just_above_it() {
        let mut terminal = terminal_after_a_shell();
        let mut queue = HistoryQueue::default();

        queue.write_frame(&mut terminal, 2).expect("frame");
        draw_prompt(&mut terminal);

        assert_eq!(terminal.viewport_area, Rect::new(0, HEIGHT - 2, WIDTH, 2));
        let rows = screen(&terminal);
        assert_eq!(rows[6], "$ nori");
        assert_eq!(rows[7], "shell output");
        assert_eq!(rows[8], "> prompt");
        assert_eq!(rows[9], "footer");
        assert!(rows[..6].iter().all(String::is_empty), "{rows:?}");
    }

    #[test]
    fn a_prompt_that_shrinks_later_does_not_move_the_screen() {
        let mut terminal = terminal_after_a_shell();
        let mut queue = HistoryQueue::default();
        queue.write_frame(&mut terminal, 4).expect("frame");
        draw_prompt(&mut terminal);
        let before = terminal.backend().scrollback_rows().len();

        // The prompt grows to four rows and shrinks back; neither puts rows into the scrollback.
        queue.write_frame(&mut terminal, 2).expect("frame");
        draw_prompt(&mut terminal);

        assert_eq!(terminal.backend().scrollback_rows().len(), before);
        assert_eq!(terminal.viewport_area.height, 2);
    }

    #[test]
    fn a_popup_covers_the_screen_and_gives_it_back_when_it_closes() {
        let mut terminal = terminal_after_a_shell();
        let mut queue = HistoryQueue::default();
        queue.write_frame(&mut terminal, 2).expect("frame");
        queue.push((0..5).map(|n| Line::from(format!("history {n}"))).collect());
        queue.write_frame(&mut terminal, 2).expect("frame");
        draw_prompt(&mut terminal);
        let resting = screen(&terminal);
        let scrolled = terminal.backend().scrollback_rows().len();

        for _ in 0..3 {
            queue
                .write_frame_with_popup(&mut terminal, 6, 4)
                .expect("frame");
            draw_prompt(&mut terminal);
        }
        assert_eq!(terminal.viewport_area, Rect::new(0, HEIGHT - 6, WIDTH, 6));
        assert_eq!(terminal.backend().scrollback_rows().len(), scrolled);

        queue
            .write_frame_with_popup(&mut terminal, 2, 0)
            .expect("frame");
        draw_prompt(&mut terminal);

        assert_eq!(terminal.viewport_area, Rect::new(0, HEIGHT - 2, WIDTH, 2));
        assert_eq!(screen(&terminal), resting);
        assert_eq!(terminal.backend().scrollback_rows().len(), scrolled);
    }

    #[test]
    fn a_popup_that_changes_between_frames_leaves_nothing_of_the_last_one() {
        let mut terminal = terminal_after_a_shell();
        let mut queue = HistoryQueue::default();
        queue.write_frame(&mut terminal, 2).expect("frame");
        let show = |terminal: &mut Terminal<VT100Backend>, rows: &[&str]| {
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    for (offset, row) in rows.iter().enumerate() {
                        frame.buffer_mut().set_string(
                            area.x,
                            area.y + offset as u16,
                            row,
                            ratatui::style::Style::default(),
                        );
                    }
                })
                .expect("draw");
        };

        queue
            .write_frame_with_popup(&mut terminal, 5, 3)
            .expect("frame");
        show(
            &mut terminal,
            &["> prompt", "Haiku 5.5", "Fastest", "Sonnet 5", "footer"],
        );
        queue
            .write_frame_with_popup(&mut terminal, 5, 3)
            .expect("frame");
        show(
            &mut terminal,
            &["> prompt", "Opus 4.5", "Best", "Opus 4.8", "footer"],
        );

        let rows = screen(&terminal);
        assert_eq!(
            rows[HEIGHT as usize - 5..],
            ["> prompt", "Opus 4.5", "Best", "Opus 4.8", "footer"]
        );
    }

    #[test]
    fn clearing_hides_everything_above_the_prompt_in_the_scrollback() {
        let mut terminal = terminal_after_a_shell();
        let mut queue = HistoryQueue::default();
        queue.write_frame(&mut terminal, 2).expect("frame");
        queue.push((0..4).map(|n| Line::from(format!("history {n}"))).collect());
        queue.write_frame(&mut terminal, 2).expect("frame");
        draw_prompt(&mut terminal);
        assert!(screen(&terminal).contains(&"history 3".to_string()));

        queue.request_clear_above();
        queue.write_frame(&mut terminal, 2).expect("frame");
        draw_prompt(&mut terminal);

        let rows = screen(&terminal);
        assert!(
            rows[..HEIGHT as usize - 2].iter().all(String::is_empty),
            "{rows:?}"
        );
        assert_eq!(rows[HEIGHT as usize - 2], "> prompt");
        // That the rows went into the scrollback rather than away is the terminal's doing (the
        // emulator behind this test does not keep rows a region scroll pushes out); it is
        // checked against a real terminal.
        assert_eq!(terminal.owned_history_rows(), 0);
    }

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
