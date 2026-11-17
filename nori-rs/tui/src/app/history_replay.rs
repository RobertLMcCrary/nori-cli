//! Restore replayed cells to native terminal history in chronological order.

use std::sync::Arc;

use super::App;
use crate::client_tool_cell::ClientToolCell;
use crate::history_cell::HistoryCell;
use crate::tui;

/// A local replay cannot still be executing a tool from the previous process.
fn finish_interrupted_tools(
    cells: &mut [Arc<dyn HistoryCell>],
    source: nori_protocol::ReplaySource,
    cloud_mode: bool,
) {
    if cloud_mode || source != nori_protocol::ReplaySource::Transcript {
        return;
    }
    for cell in cells {
        if let Some(tool) = cell.as_any().downcast_ref::<ClientToolCell>()
            && tool.is_active()
        {
            let mut completed = tool.clone();
            completed.mark_failed();
            *cell = Arc::new(completed);
        }
    }
}

impl App {
    pub(super) fn paint_replayed_history(&mut self, tui: &mut tui::Tui) {
        let Some((start, source)) = self.replay_painted_from.take() else {
            return;
        };
        let start = start.min(self.transcript_cells.len());
        finish_interrupted_tools(&mut self.transcript_cells[start..], source, self.cloud_mode);
        let replayed = self.transcript_cells[start..].to_vec();
        let mut ready = Vec::new();
        for cell in replayed {
            ready.extend(self.native_history.push(cell));
        }
        self.emit_native_history_cells(tui, ready);
        tui.frame_requester().schedule_frame();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::ToolKind;
    use crate::presentation::ToolPhase;
    use crate::presentation::ToolSnapshot;
    use std::path::PathBuf;

    #[test]
    fn local_replay_marks_only_unfinished_tools_as_interrupted() {
        let make = |phase| -> Arc<dyn HistoryCell> {
            Arc::new(ClientToolCell::new(
                ToolSnapshot {
                    call_id: "call".to_owned(),
                    title: "command".to_owned(),
                    kind: ToolKind::Execute,
                    phase,
                    locations: Vec::new(),
                    invocation: None,
                    artifacts: Vec::new(),
                    raw_input: None,
                    raw_output: None,
                    owner_request_id: None,
                },
                PathBuf::from("/"),
                false,
            ))
        };
        let active = make(ToolPhase::InProgress);
        let complete = make(ToolPhase::Completed);
        let mut cells = vec![active.clone(), complete.clone()];
        finish_interrupted_tools(&mut cells, nori_protocol::ReplaySource::Transcript, false);
        assert!(
            !cells[0]
                .as_any()
                .downcast_ref::<ClientToolCell>()
                .unwrap()
                .is_active()
        );
        assert!(
            !cells[1]
                .as_any()
                .downcast_ref::<ClientToolCell>()
                .unwrap()
                .is_active()
        );
        assert!(!Arc::ptr_eq(&cells[0], &active));
        assert!(Arc::ptr_eq(&cells[1], &complete));
        for (source, cloud) in [
            (nori_protocol::ReplaySource::Agent, false),
            (nori_protocol::ReplaySource::Transcript, true),
        ] {
            let mut remote = vec![active.clone()];
            finish_interrupted_tools(&mut remote, source, cloud);
            assert!(Arc::ptr_eq(&remote[0], &active));
        }
    }
}
