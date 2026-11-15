//! Painting a replayed history.
//!
//! Resuming a long session replays every turn of it. Writing each replayed cell to the terminal as
//! it arrives costs every row of the whole session in output, however little of it can be scrolled
//! back to. So the cells of a replay go into the transcript only, and when the replay ends the
//! newest of them are painted together, bounded the way a resize reflow bounds what it paints
//! (`transcript_reflow::render_transcript_tail`): at most the newest 1,000 cells and 10,000 rows,
//! after a `… history truncated` notice when either limit applies. The transcript keeps all of it,
//! for the transcript overlay and for later reflows.

use super::App;
use crate::tui;

impl App {
    pub(super) fn paint_replayed_history(&mut self, tui: &mut tui::Tui) {
        let Some(start) = self.replay_painted_from.take() else {
            return;
        };
        let replayed = &self.transcript_cells[start.min(self.transcript_cells.len())..];
        let width = tui.terminal.last_known_screen_size.width;
        let mut lines = crate::transcript_reflow::render_transcript_tail(replayed, width);
        if lines.is_empty() {
            return;
        }
        if self.has_emitted_history_lines {
            lines.insert(0, ratatui::text::Line::default());
        } else {
            self.has_emitted_history_lines = true;
        }
        if self.overlay.is_some() {
            self.deferred_history_lines.extend(lines);
        } else {
            tui.insert_history_lines(lines);
        }
    }
}
