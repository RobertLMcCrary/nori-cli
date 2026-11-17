//! Batching of queued events into a single frame.

use super::*;

impl App {
    /// Handle a terminal event. A draw first applies every app event that is already queued and
    /// renders the stream once, so any number of events that piled up while the previous frame was
    /// being written cost one frame, not one each.
    pub(super) async fn handle_tui_event_batched(
        &mut self,
        tui: &mut tui::Tui,
        app_event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
        event: TuiEvent,
    ) -> Result<bool> {
        if !matches!(event, TuiEvent::Draw) {
            return self.handle_tui_event(tui, event).await;
        }
        if !self.apply_queued_events(tui, app_event_rx).await? {
            return Ok(false);
        }
        // Rendering the stream may queue history cells; apply those before drawing as well.
        self.chat_widget.commit_stream_for_frame();
        if !self.apply_queued_events(tui, app_event_rx).await? {
            return Ok(false);
        }
        self.handle_tui_event(tui, event).await
    }

    async fn apply_queued_events(
        &mut self,
        tui: &mut tui::Tui,
        app_event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    ) -> Result<bool> {
        while let Ok(queued) = app_event_rx.try_recv() {
            if !self.handle_event(tui, queued).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Clear-screen binding: hide everything above the prompt, leaving scrollback alone.
    pub(super) fn repaint_screen(&mut self, tui: &mut tui::Tui) {
        // Remember the transcript boundary so a resize/focus redraw can distinguish the exact
        // clear-screen state from a later turn or a switched/backtracked session.
        self.hidden_transcript_len =
            Some(i64::try_from(self.transcript_cells.len()).unwrap_or(i64::MAX));
        if tui.is_alt_screen_active() {
            // A pager overlay draws the whole screen every frame; forgetting what the terminal
            // shows is all it takes.
            tui.request_full_repaint();
        } else {
            tui.request_clear_screen();
        }
    }
}
