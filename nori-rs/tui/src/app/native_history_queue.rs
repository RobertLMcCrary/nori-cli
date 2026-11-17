//! Hold mutable tool rows until they can be printed to immutable terminal scrollback.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::client_tool_cell::ClientToolCell;
use crate::history_cell::HistoryCell;

#[derive(Default)]
pub(super) struct NativeHistoryQueue {
    cells: VecDeque<Arc<dyn HistoryCell>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cell::UserHistoryCell;
    use crate::presentation::ToolKind;
    use crate::presentation::ToolPhase;
    use crate::presentation::ToolSnapshot;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn tool(phase: ToolPhase) -> Arc<dyn HistoryCell> {
        Arc::new(ClientToolCell::new(
            ToolSnapshot {
                call_id: "call".to_owned(),
                title: "test".to_owned(),
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
    }

    #[test]
    fn pending_tool_holds_following_cells_until_final_state() {
        let mut queue = NativeHistoryQueue::default();
        let text: Arc<dyn HistoryCell> = Arc::new(UserHistoryCell {
            message: "after".to_owned(),
        });
        assert!(queue.push(tool(ToolPhase::InProgress)).is_empty());
        assert!(queue.push(text.clone()).is_empty());
        assert_eq!(queue.cells().len(), 2);
        let ready = queue.replace_tool("call", tool(ToolPhase::Completed));
        assert_eq!(ready.len(), 2);
        assert!(Arc::ptr_eq(&ready[1], &text));
        assert!(queue.cells().is_empty());
    }

    #[test]
    fn cancelled_tool_releases_following_history() {
        let mut queue = NativeHistoryQueue::default();
        assert!(queue.push(tool(ToolPhase::InProgress)).is_empty());
        assert!(
            queue
                .push(Arc::new(UserHistoryCell {
                    message: "after".to_owned()
                }))
                .is_empty()
        );
        assert_eq!(queue.replace_tool("call", tool(ToolPhase::Failed)).len(), 2);
    }

    #[test]
    fn cancelling_group_finalizes_every_remaining_active_call() {
        let snapshot = |id: &str, phase| ToolSnapshot {
            call_id: id.to_owned(),
            title: "Read".to_owned(),
            kind: ToolKind::Read,
            phase,
            locations: Vec::new(),
            invocation: None,
            artifacts: Vec::new(),
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let mut group = ClientToolCell::new(
            snapshot("first", ToolPhase::InProgress),
            PathBuf::from("/"),
            false,
        );
        group.mark_exploring();
        group.merge_exploring(snapshot("second", ToolPhase::InProgress));
        group.apply_snapshot(snapshot("first", ToolPhase::Completed));
        assert!(group.is_active());
        let mut queue = NativeHistoryQueue::default();
        assert!(queue.push(Arc::new(group.clone())).is_empty());
        assert_eq!(queue.active_tool_ids(), vec!["first"]);
        group.mark_failed();
        assert!(!group.is_active());
        assert_eq!(queue.replace_tool("first", Arc::new(group)).len(), 1);
        assert!(queue.cells().is_empty());
    }

    #[test]
    fn transcript_rewrite_and_rewind_replace_the_unprinted_suffix() {
        let mut queue = NativeHistoryQueue::default();
        let running = tool(ToolPhase::InProgress);
        let original: Arc<dyn HistoryCell> = Arc::new(UserHistoryCell {
            message: "old".to_owned(),
        });
        let replacement: Arc<dyn HistoryCell> = Arc::new(UserHistoryCell {
            message: "new".to_owned(),
        });
        assert!(queue.push(running.clone()).is_empty());
        assert!(queue.push(original).is_empty());
        assert!(
            queue
                .retain_transcript(&[running.clone(), replacement.clone()])
                .is_empty()
        );
        assert!(Arc::ptr_eq(&queue.cells()[1], &replacement));
        assert!(queue.retain_transcript(&[running]).is_empty());
        assert_eq!(queue.cells().len(), 1);
        assert_eq!(
            queue.replace_tool("call", tool(ToolPhase::Completed)).len(),
            1
        );
    }
}

impl NativeHistoryQueue {
    pub(super) fn push(&mut self, cell: Arc<dyn HistoryCell>) -> Vec<Arc<dyn HistoryCell>> {
        self.cells.push_back(cell);
        self.take_ready()
    }

    pub(super) fn replace_tool(
        &mut self,
        call_id: &str,
        replacement: Arc<dyn HistoryCell>,
    ) -> Vec<Arc<dyn HistoryCell>> {
        if let Some(cell) = self.cells.iter_mut().find(|cell| {
            cell.as_any()
                .downcast_ref::<ClientToolCell>()
                .is_some_and(|tool| {
                    tool.call_id() == call_id
                        || tool.exploring_call_ids().iter().any(|id| id == call_id)
                })
        }) {
            *cell = replacement;
        }
        self.take_ready()
    }

    pub(super) fn cells(&self) -> &VecDeque<Arc<dyn HistoryCell>> {
        &self.cells
    }

    pub(super) fn tool_index(&self, call_id: &str) -> Option<usize> {
        self.cells.iter().position(|cell| {
            cell.as_any()
                .downcast_ref::<ClientToolCell>()
                .is_some_and(|tool| {
                    tool.call_id() == call_id
                        || tool.exploring_call_ids().iter().any(|id| id == call_id)
                })
        })
    }

    pub(super) fn active_tool_ids(&self) -> Vec<String> {
        self.cells
            .iter()
            .filter_map(|cell| {
                let tool = cell.as_any().downcast_ref::<ClientToolCell>()?;
                tool.is_active().then(|| tool.call_id().to_owned())
            })
            .collect()
    }

    /// Reconcile after a transcript rewrite or truncation, keeping the unprinted suffix.
    pub(super) fn retain_transcript(
        &mut self,
        transcript: &[Arc<dyn HistoryCell>],
    ) -> Vec<Arc<dyn HistoryCell>> {
        let start = self
            .cells
            .iter()
            .find_map(|queued| transcript.iter().position(|cell| Arc::ptr_eq(queued, cell)));
        self.cells = start.map_or_else(VecDeque::new, |index| {
            transcript[index..].iter().cloned().collect()
        });
        self.take_ready()
    }

    fn take_ready(&mut self) -> Vec<Arc<dyn HistoryCell>> {
        let mut ready = Vec::new();
        while self.cells.front().is_some_and(|cell| {
            !cell
                .as_any()
                .downcast_ref::<ClientToolCell>()
                .is_some_and(ClientToolCell::is_active)
        }) {
            if let Some(cell) = self.cells.pop_front() {
                ready.push(cell);
            }
        }
        ready
    }
}
