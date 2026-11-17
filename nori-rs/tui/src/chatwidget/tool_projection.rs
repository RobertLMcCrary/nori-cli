//! Per-tool row ranges in a rendered conversation.

use crate::client_tool_cell::ClientToolCell;
use crate::history_cell::HistoryCell;
use ratatui::text::Line;
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub(crate) struct ToolSpan {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) call_id: String,
}

#[derive(Default)]
pub(crate) struct ToolProjection {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) spans: Vec<ToolSpan>,
}

impl ToolProjection {
    pub(crate) fn push(&mut self, cell: &dyn HistoryCell, width: u16, expanded: &HashSet<String>) {
        self.push_with_mode(cell, width, expanded, false);
    }

    pub(crate) fn push_main(
        &mut self,
        cell: &dyn HistoryCell,
        width: u16,
        expanded: &HashSet<String>,
    ) {
        self.push_with_mode(cell, width, expanded, true);
    }

    fn push_with_mode(
        &mut self,
        cell: &dyn HistoryCell,
        width: u16,
        expanded: &HashSet<String>,
        main: bool,
    ) {
        if !self.lines.is_empty() && !cell.is_stream_continuation() {
            self.lines.push(Line::default());
        }
        let start = self.lines.len();
        if let Some(tool) = cell.as_any().downcast_ref::<ClientToolCell>() {
            let call_id = tool.call_id().to_owned();
            let rendered = if expanded.contains(&call_id) {
                tool.expanded_lines(width)
            } else {
                tool.compact_lines(width)
            };
            self.lines.extend(rendered);
            if self.lines.len() > start {
                self.spans.push(ToolSpan {
                    start,
                    end: self.lines.len(),
                    call_id,
                });
            }
        } else if main {
            self.lines.extend(cell.display_lines(width));
        } else {
            self.lines
                .extend(crate::subagent_switcher::cell_lines(cell, width));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::ToolKind;
    use crate::presentation::ToolPhase;
    use crate::presentation::ToolSnapshot;
    use pretty_assertions::assert_eq;

    fn tool(id: &str) -> ClientToolCell {
        ClientToolCell::new(
            ToolSnapshot {
                call_id: id.to_owned(),
                title: format!("Tool {id}"),
                kind: ToolKind::Read,
                phase: ToolPhase::Completed,
                locations: Vec::new(),
                invocation: None,
                artifacts: Vec::new(),
                raw_input: Some(serde_json::json!({"path":"/tmp/example", "options": [1, 2, 3]})),
                raw_output: Some(serde_json::json!({"result":"a detailed output"})),
                owner_request_id: None,
            },
            std::path::PathBuf::from("/tmp"),
            false,
        )
    }

    #[test]
    fn expanding_one_tool_moves_following_hit_ranges() {
        let first = tool("first");
        let second = tool("second");
        let mut compact = ToolProjection::default();
        compact.push(&first, 40, &HashSet::new());
        compact.push(&second, 40, &HashSet::new());
        let mut expanded = ToolProjection::default();
        expanded.push(&first, 40, &HashSet::from(["first".to_owned()]));
        expanded.push(&second, 40, &HashSet::from(["first".to_owned()]));
        assert_eq!(
            compact
                .spans
                .iter()
                .map(|span| span.call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert!(expanded.spans[1].start > compact.spans[1].start);
    }
}
