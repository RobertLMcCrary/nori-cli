//! Repainting the screen from Nori's own model (Ctrl+L).
//!
//! Terminals can end up showing something other than what Nori drew: a program that wrote to the
//! same tty, a resize that the terminal reflowed differently, a font change. Redrawing the whole
//! screen is not an option, because the rows above the viewport that Nori did not write are the
//! user's own history, and a full clear (`ESC[2J` pushes the screen into scrollback on some
//! terminals, `ESC[3J` deletes scrollback on all of them) would damage it.
//!
//! So a repaint touches exactly the rows Nori owns: the viewport and the history it wrote above it
//! that is still on screen. Those are cleared and written again from the transcript, in the same
//! synchronized update as the next frame.

use ratatui::text::Line;

use super::Tui;

impl Tui {
    /// Rows a repaint has to replay: the history Nori wrote that is still above the viewport,
    /// plus history that was queued and not written yet.
    pub(crate) fn history_rows_to_repaint(&self) -> std::io::Result<usize> {
        let width = self.terminal.size()?.width;
        let pending = crate::insert_history::physical_rows_of(&self.pending_history_lines, width);
        Ok(self.terminal.owned_history_rows() + pending)
    }

    /// Repaint the screen with `history_tail` (the newest history rows, oldest first) above the
    /// viewport on the next draw. Scrollback is never touched.
    pub(crate) fn request_repaint(&mut self, history_tail: Vec<Line<'static>>) {
        self.repaint_history = Some(history_tail);
        self.frame_requester().schedule_frame();
    }

    /// Repaint the whole alternate screen (a pager overlay owns all of it) on the next draw.
    pub(crate) fn request_full_repaint(&mut self) {
        self.terminal.invalidate();
        self.full_repaint = true;
        self.frame_requester().schedule_frame();
    }

    /// Start the repaint inside the frame being drawn: clear what Nori owns, move the viewport to
    /// where the replayed history will start, and queue that history for insertion.
    pub(super) fn begin_repaint(&mut self) -> std::io::Result<()> {
        if std::mem::take(&mut self.full_repaint) {
            self.terminal.clear()?;
        }
        let Some(history_tail) = self.repaint_history.take() else {
            return Ok(());
        };
        let width = self.terminal.size()?.width;
        let rows = crate::insert_history::physical_rows_of(&history_tail, width);
        restart_viewport_for_repaint(&mut self.terminal, rows)?;
        // The tail was cut from the transcript, which already contains everything that was still
        // waiting to be written.
        self.pending_history_lines = history_tail;
        Ok(())
    }
}

/// Clear the rows Nori owns and move the viewport up over them, ready for `history_rows` rows of
/// history to be written again above it.
///
/// Writing the history makes room for itself by scrolling the viewport back down, so afterwards
/// the viewport is where it was and nothing above the cleared rows has moved or been written to.
pub(crate) fn restart_viewport_for_repaint<B>(
    terminal: &mut crate::custom_terminal::Terminal<B>,
    history_rows: usize,
) -> std::io::Result<()>
where
    B: ratatui::backend::Backend<Error = std::io::Error> + std::io::Write,
{
    let mut area = terminal.viewport_area;
    area.y = area
        .top()
        .saturating_sub(u16::try_from(history_rows).unwrap_or(u16::MAX));
    terminal.set_viewport_area(area);
    terminal.clear()?;
    terminal.reset_history_rows();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insert_history::insert_history_lines;
    use crate::test_backend::VT100Backend;
    use pretty_assertions::assert_eq;
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use std::io::Write;

    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 12;
    const VIEWPORT_ROWS: u16 = 3;

    fn history(count: usize) -> Vec<Line<'static>> {
        (0..count)
            .map(|index| Line::from(format!("History line {index}")))
            .collect()
    }

    fn draw_composer(terminal: &mut crate::custom_terminal::Terminal<VT100Backend>) {
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame
                    .buffer_mut()
                    .set_string(area.x, area.y, "> composer", Style::default());
                frame
                    .buffer_mut()
                    .set_string(area.x, area.y + 2, "footer", Style::default());
            })
            .expect("draw");
    }

    fn screen_rows(terminal: &mut crate::custom_terminal::Terminal<VT100Backend>) -> Vec<String> {
        terminal.backend().vt100().screen().rows(0, WIDTH).collect()
    }

    /// A terminal whose rows above the viewport hold text Nori never wrote, with `count` rows of
    /// history that it did written below that, and the composer.
    fn terminal_with_history(count: usize) -> crate::custom_terminal::Terminal<VT100Backend> {
        let backend = VT100Backend::with_scrollback(WIDTH, HEIGHT, 500);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        for row in 0..HEIGHT - VIEWPORT_ROWS {
            write!(
                terminal.backend_mut(),
                "\x1b[{};1Hthe user's row {row}",
                row + 1
            )
            .expect("foreign text");
        }
        terminal.set_viewport_area(Rect::new(0, HEIGHT - VIEWPORT_ROWS, WIDTH, VIEWPORT_ROWS));
        insert_history_lines(&mut terminal, &history(count)).expect("insert");
        draw_composer(&mut terminal);
        terminal
    }

    /// Damage the owned rows (the newest three above the viewport, rows 6 to 8) and the viewport.
    fn corrupt(terminal: &mut crate::custom_terminal::Terminal<VT100Backend>) {
        let garbage: &[u8] = b"\x1b[7;1H\x1b[31mGARBAGE\x1b[0m\x1b[8;3Hxxxxxxxxxxxxxxxxxxxxxx\x1b[9;1H\x1b[2Kyyyyyy\x1b[11;5Hzzzz\x1b[12;1Hqqqq";
        terminal.backend_mut().write_all(garbage).expect("corrupt");
    }

    /// Replay the newest `count` rows of the 3-line history, as the transcript would supply them.
    fn replay(
        terminal: &mut crate::custom_terminal::Terminal<VT100Backend>,
        count: usize,
    ) -> Vec<u8> {
        let tail: Vec<Line<'static>> = history(3).split_off(3 - count);
        terminal.backend_mut().start_recording();
        restart_viewport_for_repaint(terminal, count).expect("restart");
        insert_history_lines(terminal, &tail).expect("replay");
        draw_composer(terminal);
        terminal.backend_mut().take_recording()
    }

    #[test]
    fn repaint_restores_a_corrupted_screen_and_leaves_rows_above_it_alone() {
        let mut terminal = terminal_with_history(3);
        let fresh = screen_rows(&mut terminal);
        let owned = terminal.owned_history_rows();
        assert_eq!(owned, 3);
        assert_eq!(fresh[0], "the user's row 3");
        assert_eq!(fresh[6], "History line 0");

        corrupt(&mut terminal);
        let corrupted = screen_rows(&mut terminal);
        assert!(corrupted != fresh, "the screen should be corrupted");

        replay(&mut terminal, owned);

        assert_eq!(screen_rows(&mut terminal), fresh);
        assert_eq!(terminal.owned_history_rows(), owned);
    }

    #[test]
    fn repaint_never_emits_the_full_screen_clears() {
        let mut terminal = terminal_with_history(3);
        let owned = terminal.owned_history_rows();
        let before = screen_rows(&mut terminal);

        let recorded = replay(&mut terminal, owned);

        let text = String::from_utf8_lossy(&recorded);
        assert!(!text.contains("\x1b[2J"), "{text:?}");
        assert!(!text.contains("\x1b[3J"), "{text:?}");
        // Repainting a screen that was fine changes nothing, including the rows above the owned
        // region.
        assert_eq!(screen_rows(&mut terminal), before);
    }

    #[test]
    fn repainting_an_empty_history_clears_only_the_viewport() {
        let backend = VT100Backend::with_scrollback(WIDTH, HEIGHT, 500);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        terminal.set_viewport_area(Rect::new(0, HEIGHT - VIEWPORT_ROWS, WIDTH, VIEWPORT_ROWS));
        draw_composer(&mut terminal);
        let fresh = screen_rows(&mut terminal);

        corrupt(&mut terminal);
        restart_viewport_for_repaint(&mut terminal, 0).expect("restart");
        draw_composer(&mut terminal);

        let repainted = screen_rows(&mut terminal);
        // Rows above the viewport were never Nori's, so only the viewport is back to normal.
        assert_eq!(
            repainted[usize::from(HEIGHT - VIEWPORT_ROWS)..],
            fresh[usize::from(HEIGHT - VIEWPORT_ROWS)..]
        );
    }
}
