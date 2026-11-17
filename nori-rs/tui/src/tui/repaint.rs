//! Hiding everything above the prompt, and the repaint the tests exercise.
//!
//! The clear-screen action scrolls rows above the viewport into scrollback, which is left whole, and
//! leaves the screen above the prompt empty. A full clear is not used: `ESC[2J` pushes the screen
//! into scrollback on some terminals and `ESC[3J` deletes scrollback on all of them.
//!
//! A repaint, kept for recovering a screen that shows something other than what Nori drew,
//! touches exactly the rows Nori owns: the viewport, and the history it wrote directly above it
//! that is still on screen. Those history rows are written again where they are, from the
//! terminal's record of what it wrote there, in the same synchronized update as the next frame
//! (see `history_queue`). History still waiting to be written is written after them as usual, so
//! a repaint in the middle of a stream can neither drop nor repeat a line.

use super::Tui;

impl Tui {
    /// Hide everything above the prompt on the next draw: it scrolls into the scrollback, which
    /// is left whole, and the prompt stays at the bottom of an empty screen.
    pub(crate) fn request_clear_screen(&mut self) {
        self.history.request_clear_above();
        self.frame_requester().schedule_frame();
    }

    /// Repaint the current viewport on the next draw.
    pub(crate) fn request_full_repaint(&mut self) {
        self.terminal.invalidate();
        self.full_repaint = true;
        self.frame_requester().schedule_frame();
    }
}

#[cfg(test)]
mod tests {
    use super::super::history_queue::HistoryQueue;
    use crate::custom_terminal::Terminal;
    use crate::history_cell::HistoryCell;
    use crate::insert_history::insert_history_lines;
    use crate::streaming::controller::StreamController;
    use crate::test_backend::VT100Backend;
    use pretty_assertions::assert_eq;
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use ratatui::text::Line;
    use std::io::Write;

    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 12;
    const VIEWPORT_ROWS: u16 = 3;

    fn history(count: usize) -> Vec<Line<'static>> {
        (0..count)
            .map(|index| Line::from(format!("History line {index}")))
            .collect()
    }

    fn draw_composer(terminal: &mut Terminal<VT100Backend>) {
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

    fn screen_rows(terminal: &Terminal<VT100Backend>) -> Vec<String> {
        terminal.backend().vt100().screen().rows(0, WIDTH).collect()
    }

    /// A terminal whose rows above the viewport hold text Nori never wrote, with `count` rows of
    /// history that it did written below that, and the composer.
    fn terminal_with_history(count: usize) -> Terminal<VT100Backend> {
        let backend = VT100Backend::with_scrollback(WIDTH, HEIGHT);
        let mut terminal = Terminal::with_options(backend).expect("terminal");
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
    fn corrupt(terminal: &mut Terminal<VT100Backend>) {
        let garbage: &[u8] = b"\x1b[7;1H\x1b[31mGARBAGE\x1b[0m\x1b[8;3Hxxxxxxxxxxxxxxxxxxxxxx\x1b[9;1H\x1b[2Kyyyyyy\x1b[11;5Hzzzz\x1b[12;1Hqqqq";
        terminal.backend_mut().write_all(garbage).expect("corrupt");
    }

    /// Run the next frame with a repaint requested, returning what it wrote.
    fn repaint(terminal: &mut Terminal<VT100Backend>) -> Vec<u8> {
        let mut queue = HistoryQueue::default();
        queue.request_repaint();
        terminal.backend_mut().start_recording();
        queue.write_frame(terminal, VIEWPORT_ROWS).expect("frame");
        draw_composer(terminal);
        terminal.backend_mut().take_recording()
    }

    #[test]
    fn repaint_restores_a_corrupted_screen_and_leaves_rows_above_it_alone() {
        let mut terminal = terminal_with_history(3);
        let fresh = screen_rows(&terminal);
        let owned = terminal.owned_history_rows();
        assert_eq!(owned, 3);
        assert_eq!(fresh[0], "the user's row 3");
        assert_eq!(fresh[6], "History line 0");

        corrupt(&mut terminal);
        let corrupted = screen_rows(&terminal);
        assert!(corrupted != fresh, "the screen should be corrupted");

        repaint(&mut terminal);

        assert_eq!(screen_rows(&terminal), fresh);
        assert_eq!(terminal.owned_history_rows(), owned);
    }

    #[test]
    fn repaint_never_emits_the_full_screen_clears() {
        let mut terminal = terminal_with_history(3);
        let before = screen_rows(&terminal);
        let scrollback = terminal.backend().scrollback_rows().to_vec();

        let recorded = repaint(&mut terminal);

        let text = String::from_utf8_lossy(&recorded);
        assert!(!text.contains("\x1b[2J"), "{text:?}");
        assert!(!text.contains("\x1b[3J"), "{text:?}");
        // Repainting a screen that was fine changes nothing, including the rows above the owned
        // region and the scrollback.
        assert_eq!(screen_rows(&terminal), before);
        assert_eq!(terminal.backend().scrollback_rows(), scrollback.as_slice());
    }

    #[test]
    fn repainting_an_empty_history_clears_only_the_viewport() {
        let backend = VT100Backend::with_scrollback(WIDTH, HEIGHT);
        let mut terminal = Terminal::with_options(backend).expect("terminal");
        terminal.set_viewport_area(Rect::new(0, HEIGHT - VIEWPORT_ROWS, WIDTH, VIEWPORT_ROWS));
        draw_composer(&mut terminal);
        let fresh = screen_rows(&terminal);

        corrupt(&mut terminal);
        repaint(&mut terminal);

        let repainted = screen_rows(&terminal);
        // Rows above the viewport were never Nori's, so only the viewport is back to normal.
        assert_eq!(
            repainted[usize::from(HEIGHT - VIEWPORT_ROWS)..],
            fresh[usize::from(HEIGHT - VIEWPORT_ROWS)..]
        );
    }

    /// What a streamed answer looks like in the terminal: everything that scrolled off the top,
    /// then every row of the screen, each with its styles.
    type Rendered = (Vec<(String, Vec<u8>)>, Vec<Vec<u8>>);

    const STREAM_WIDTH: u16 = 40;
    const STREAM_HEIGHT: u16 = 16;
    /// Rows at the top of the screen that hold the user's shell output when Nori starts.
    const FOREIGN_ROWS: u16 = 5;
    /// Frames, counted from the start, after which a burst of tool cells lands mid-stream.
    const TOOL_BURSTS: [usize; 2] = [9, 31];
    const ANSWER: &str = "Here is the plan, in a paragraph long enough to wrap across a few rows of the narrow terminal.\n\n1. First step with **bold** words\n2. Second step\n   - nested detail one\n   - nested detail two\n\n```rust\nfn main() {\n    let total = 1 + 2;\n    println!(\"{total}\");\n}\n```\n\n> A quoted remark that also wraps once at forty columns.\n\nAnd a closing paragraph that streams in many small chunks, with `code` and _emphasis_ in it, until it ends.\n";

    #[derive(Clone, Copy, Debug)]
    enum RepaintTiming {
        /// The key is handled before the frame commits the stream, as it is when it arrives
        /// between two frames.
        BeforeCommit,
        /// The key is handled after the frame's commit was queued.
        AfterCommit,
    }

    /// The app's side of one streamed answer: it queues committed cells as `App` does for
    /// `InsertHistoryCell` and draws the live tail, a subagent block whose height changes, and
    /// the composer in the viewport.
    struct Session {
        terminal: Terminal<VT100Backend>,
        queue: HistoryQueue,
        has_emitted_history: bool,
    }

    impl Session {
        fn new() -> Self {
            let backend = VT100Backend::with_scrollback(STREAM_WIDTH, STREAM_HEIGHT);
            let mut terminal = Terminal::with_options(backend).expect("terminal");
            for row in 0..FOREIGN_ROWS {
                write!(
                    terminal.backend_mut(),
                    "\x1b[{};1H\x1b[32m$ the user's row {row}\x1b[0m",
                    row + 1
                )
                .expect("foreign text");
            }
            terminal.set_viewport_area(Rect::new(0, FOREIGN_ROWS, STREAM_WIDTH, 2));
            Self {
                terminal,
                queue: HistoryQueue::default(),
                has_emitted_history: false,
            }
        }

        fn insert_cell(&mut self, cell: &dyn HistoryCell) {
            let mut lines = cell.display_lines(STREAM_WIDTH);
            if lines.is_empty() {
                return;
            }
            if !cell.is_stream_continuation() {
                if self.has_emitted_history {
                    lines.insert(0, Line::default());
                } else {
                    self.has_emitted_history = true;
                }
            }
            self.queue.push(lines);
        }

        /// Write garbage over every row a repaint is responsible for.
        fn corrupt_owned_rows(&mut self) {
            let top = self.terminal.viewport_area.top();
            let owned = u16::try_from(self.terminal.owned_history_rows()).expect("rows");
            for row in top - owned..STREAM_HEIGHT {
                write!(
                    self.terminal.backend_mut(),
                    "\x1b[{};1H\x1b[31mGARBAGE {row} GARBAGE\x1b[0m",
                    row + 1
                )
                .expect("corrupt");
            }
        }

        fn draw(&mut self, live: Vec<Line<'static>>, subagent_rows: usize) {
            let mut rows = live;
            rows.extend(
                (0..subagent_rows).map(|index| Line::from(format!("  └ subagent {index}"))),
            );
            rows.push(Line::from("› composer"));
            rows.push(Line::from("  footer"));
            let height = u16::try_from(rows.len()).expect("rows");
            self.queue
                .write_frame(&mut self.terminal, height)
                .expect("frame");
            self.terminal
                .draw(|frame| {
                    let area = frame.area();
                    for (offset, line) in rows.iter().enumerate() {
                        let y = area.y + u16::try_from(offset).expect("row");
                        if y < area.bottom() {
                            frame.buffer_mut().set_line(area.x, y, line, area.width);
                        }
                    }
                })
                .expect("draw");
        }

        fn rendered(&self) -> Rendered {
            let screen = self.terminal.backend().vt100().screen();
            (
                self.terminal.backend().scrollback_rows().to_vec(),
                screen.rows_formatted(0, STREAM_WIDTH).collect(),
            )
        }
    }

    /// Stream the answer in 7-byte chunks, one frame per chunk, with tool bursts and subagent
    /// activity, and repaint at `repaint` if given.
    fn stream_answer(repaint: Option<(usize, RepaintTiming)>) -> Rendered {
        let mut session = Session::new();
        let width = Some(usize::from(STREAM_WIDTH) - 2);
        let mut stream = StreamController::new(width);
        let chunks: Vec<&str> = ANSWER
            .as_bytes()
            .chunks(7)
            .map(|chunk| std::str::from_utf8(chunk).expect("ascii"))
            .collect();
        for (frame, chunk) in chunks.iter().enumerate() {
            stream.push(chunk);
            if TOOL_BURSTS.contains(&frame) {
                // A tool call ends the message so far; its cells land in one burst, and the
                // answer continues as a new message.
                if let Some(cell) = stream.finalize().0 {
                    session.insert_cell(cell.as_ref());
                }
                for tool in 0..3 {
                    let lines = (0..4)
                        .map(|row| Line::from(format!("• tool {frame}.{tool} output row {row}")))
                        .collect();
                    session.insert_cell(&crate::history_cell::PlainHistoryCell::new(lines));
                }
                stream = StreamController::new(width);
            }
            let repaint_now = repaint.filter(|(at, _)| *at == frame);
            if let Some((_, RepaintTiming::BeforeCommit)) = repaint_now {
                session.corrupt_owned_rows();
                session.queue.request_repaint();
            }
            if let Some(cell) = stream.commit_for_frame() {
                session.insert_cell(cell.as_ref());
            }
            if let Some((_, RepaintTiming::AfterCommit)) = repaint_now {
                session.corrupt_owned_rows();
                session.queue.request_repaint();
            }
            let live = stream
                .live_tail_cell()
                .map(|cell| cell.display_lines(STREAM_WIDTH))
                .unwrap_or_default();
            session.draw(live, frame % 4);
        }
        if let Some(cell) = stream.finalize().0 {
            session.insert_cell(cell.as_ref());
        }
        session.draw(Vec::new(), 0);
        session.rendered()
    }

    #[test]
    fn a_repaint_mid_stream_neither_drops_nor_repeats_a_line() {
        let expected = stream_answer(None);
        let frames = ANSWER.len().div_ceil(7);
        // The stream scrolled the user's rows and its own first lines into the scrollback, so
        // the comparison covers rows a repaint must not touch as well as rows it must restore.
        assert!(
            expected.0.len() > usize::from(FOREIGN_ROWS) + 10,
            "{:?}",
            expected.0
        );
        // The prompt is pinned to the bottom, so blank rows may lead the user's own.
        assert!(
            expected
                .0
                .iter()
                .any(|row| row.0.starts_with("$ the user's row 0")),
            "{:?}",
            expected.0
        );
        for frame in 0..frames {
            for timing in [RepaintTiming::BeforeCommit, RepaintTiming::AfterCommit] {
                let repainted = stream_answer(Some((frame, timing)));
                assert_eq!(
                    repainted, expected,
                    "repaint {timing:?} at frame {frame} of {frames}"
                );
            }
        }
    }
}
