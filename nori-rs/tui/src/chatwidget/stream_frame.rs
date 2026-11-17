//! Per-frame work for the streaming answer.
//!
//! Deltas only append to the stream controller. Once per frame the app asks the widget to render
//! what arrived: lines that became final go to the scrollback in one cell, and the rest is shown as
//! the live tail above the composer. The cost of a frame therefore does not depend on how many
//! deltas arrived since the previous one.

use super::*;

impl ChatWidget {
    /// Drain only the unfinished portion of a streaming response before exit.
    pub(crate) fn take_stream_cell_for_exit(&mut self) -> Option<Box<dyn HistoryCell>> {
        let mut controller = self.stream_controller.take()?;
        let cell = controller.finish_for_exit();
        self.live_tail.set(None);
        cell
    }

    /// Move the last visible cell into native history before the event loop exits.
    pub(crate) fn take_active_cell_for_exit(&mut self) -> Option<Box<dyn HistoryCell>> {
        self.active_cell.take()
    }

    /// Render the stream's pending input. Called before every draw.
    pub(crate) fn commit_stream_for_frame(&mut self) {
        let Some(controller) = self.stream_controller.as_mut() else {
            return;
        };
        if !controller.has_unrendered_input() {
            return;
        }
        let committed = controller.commit_for_frame();
        let live = controller.live_tail_cell();
        // A burst too big for one frame is rendered by the frames that follow it straight away.
        if controller.has_unrendered_input() {
            self.request_redraw();
        }
        if let Some(cell) = committed {
            // NOTE: Do NOT hide the status indicator here. The "Working (Xs)" message stays
            // visible until the turn completes.
            self.add_boxed_history(cell);
        }
        self.live_tail_has_header = live
            .as_ref()
            .is_some_and(|cell| !cell.is_stream_continuation());
        self.live_tail
            .set(live.map(|cell| Box::new(cell) as Box<dyn HistoryCell>));
        // The turn is over and so, now, is the rendering of its answer.
        if self.stream_ending
            && !self
                .stream_controller
                .as_ref()
                .is_some_and(StreamController::has_unrendered_input)
        {
            self.flush_answer_stream_with_separator();
        }
    }

    /// Tell the widget how many rows the terminal has, so the viewport leaves room for history.
    pub(crate) fn set_screen_rows(&self, rows: u16) {
        self.screen_rows.set(rows);
    }
}
