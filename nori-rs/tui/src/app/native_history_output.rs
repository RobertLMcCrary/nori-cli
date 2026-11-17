//! Write completed cells to immutable primary-screen history in order.

use std::sync::Arc;

use ratatui::text::Line;

use super::App;
use crate::history_cell::HistoryCell;
use crate::render::renderable::Renderable;
use crate::subagent_switcher::Target;
use crate::tui::Tui;

impl App {
    pub(super) fn finish_native_history(
        &mut self,
        tui: &mut Tui,
    ) -> color_eyre::eyre::Result<bool> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        let mut can_emit = true;
        if !self.deferred_history_lines.is_empty() {
            tui.insert_history_lines(std::mem::take(&mut self.deferred_history_lines));
        }
        if let Some(cell) = self.chat_widget.take_stream_cell_for_exit() {
            let cell: Arc<dyn HistoryCell> = cell.into();
            self.transcript_cells.push(cell.clone());
            let ready = self.native_history.push(cell);
            if can_emit {
                can_emit = self.emit_exit_history_cells(tui, ready, deadline);
            }
        }
        if let Some(cell) = self.chat_widget.take_active_cell_for_exit() {
            let cell: Arc<dyn HistoryCell> = cell.into();
            self.transcript_cells.push(cell.clone());
            let ready = self.native_history.push(cell);
            if can_emit {
                can_emit = self.emit_exit_history_cells(tui, ready, deadline);
            }
        }
        for id in self.native_history.active_tool_ids() {
            if let Some(index) = super::tool_history::fail_active(&mut self.transcript_cells, &id) {
                let ready = self
                    .native_history
                    .replace_tool(&id, self.transcript_cells[index].clone());
                if can_emit {
                    can_emit = self.emit_exit_history_cells(tui, ready, deadline);
                }
            }
        }
        if !self.deferred_history_lines.is_empty() {
            tui.insert_history_lines(std::mem::take(&mut self.deferred_history_lines));
        }
        // Exit must remain responsive even if a tool held a large amount of output.
        while tui.has_pending_history() && std::time::Instant::now() < deadline {
            let before = tui.pending_history_line_count();
            let screen = tui.terminal.size()?;
            let height = self
                .chat_widget
                .desired_height(screen.width)
                .min(screen.height.saturating_sub(2).max(1));
            tui.draw(height, 0, |frame| {
                self.chat_widget.render(frame.area(), frame.buffer)
            })?;
            if tui.pending_history_line_count() >= before {
                break;
            }
        }
        Ok(can_emit && !tui.has_pending_history())
    }

    fn emit_exit_history_cells(
        &mut self,
        tui: &mut Tui,
        cells: Vec<Arc<dyn HistoryCell>>,
        deadline: std::time::Instant,
    ) -> bool {
        for cell in cells {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            self.emit_native_history_cells(tui, vec![cell]);
        }
        true
    }

    pub(super) fn emit_native_history_cells(
        &mut self,
        tui: &mut Tui,
        cells: Vec<Arc<dyn HistoryCell>>,
    ) {
        let width = tui.terminal.last_known_screen_size.width;
        let mut lines = Vec::new();
        for cell in cells {
            let display = cell.native_lines(width);
            if display.is_empty() {
                continue;
            }
            if !cell.is_stream_continuation() && self.has_emitted_history_lines {
                lines.push(Line::default());
            }
            self.has_emitted_history_lines = true;
            lines.extend(display);
        }
        if lines.is_empty() {
            return;
        }
        if self.overlay.is_some()
            || self.chat_widget.has_active_overlay_or_popup()
            || self.backtrack.response_focus.is_some()
            || self.session_view.main_scrolling()
            || self.session_view.focused() != Target::Main
        {
            self.deferred_history_lines.extend(lines);
        } else {
            tui.insert_history_lines(lines);
        }
    }
}
