//! Let the terminal reflow its own primary-screen history.

use super::App;

impl App {
    pub(super) fn handle_resize_reflow_draw(&mut self) {
        self.transcript_reflow.cancel();
    }
}
