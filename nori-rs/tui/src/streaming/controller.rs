use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::markdown_stream::MarkdownStreamCollector;
use ratatui::text::Line;

/// Controller for one streaming agent message.
///
/// Deltas are appended as they arrive. Once per frame the owner calls
/// [`Self::commit_for_frame`]: every line that became final since the previous frame is returned
/// in one cell for the scrollback, and [`Self::live_tail_cell`] shows the line still being typed.
/// Nothing is paced by a timer, so a burst of any size is on screen one frame after it arrived.
pub(crate) struct StreamController {
    collector: MarkdownStreamCollector,
    header_emitted: bool,
}

impl StreamController {
    pub(crate) fn new(width: Option<usize>) -> Self {
        Self {
            collector: MarkdownStreamCollector::new(width),
            header_emitted: false,
        }
    }

    /// Append a delta. Costs the length of the delta.
    pub(crate) fn push(&mut self, delta: &str) {
        self.collector.push_delta(delta);
    }

    /// Whether deltas arrived that no frame has rendered yet.
    pub(crate) fn has_unrendered_input(&self) -> bool {
        self.collector.is_dirty()
    }

    /// Render what arrived since the last frame. Returns the newly final lines as one cell.
    pub(crate) fn commit_for_frame(&mut self) -> Option<Box<dyn HistoryCell>> {
        let lines = self.collector.advance();
        self.emit(lines)
    }

    /// The rendered but not yet final lines, with the prefix their committed form will have.
    pub(crate) fn live_tail_cell(&self) -> Option<AgentMessageCell> {
        let lines = self.collector.live_tail();
        (!lines.is_empty()).then(|| AgentMessageCell::new(lines.to_vec(), !self.header_emitted))
    }

    /// Finalize the active stream. Drain and emit now.
    pub(crate) fn finalize(&mut self) -> (Option<Box<dyn HistoryCell>>, Option<String>) {
        let (lines, source) = self.collector.finalize_and_drain();
        let cell = self.emit(lines);
        let source = (!source.is_empty()).then_some(source);
        (cell, source)
    }

    fn emit(&mut self, lines: Vec<Line<'static>>) -> Option<Box<dyn HistoryCell>> {
        if lines.is_empty() {
            return None;
        }
        let is_first_line = !self.header_emitted;
        self.header_emitted = true;
        Some(Box::new(AgentMessageCell::new(lines, is_first_line)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_to_plain_strings(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    #[tokio::test]
    async fn controller_loose_vs_tight_with_commit_ticks_matches_full() {
        let mut ctrl = StreamController::new(None);
        let mut lines = Vec::new();

        // Exact deltas from the session log (section: Loose vs. tight list items)
        let deltas = vec![
            "\n\n",
            "Loose",
            " vs",
            ".",
            " tight",
            " list",
            " items",
            ":\n",
            "1",
            ".",
            " Tight",
            " item",
            "\n",
            "2",
            ".",
            " Another",
            " tight",
            " item",
            "\n\n",
            "1",
            ".",
            " Loose",
            " item",
            " with",
            " its",
            " own",
            " paragraph",
            ".\n\n",
            "  ",
            " This",
            " paragraph",
            " belongs",
            " to",
            " the",
            " same",
            " list",
            " item",
            ".\n\n",
            "2",
            ".",
            " Second",
            " loose",
            " item",
            " with",
            " a",
            " nested",
            " list",
            " after",
            " a",
            " blank",
            " line",
            ".\n\n",
            "  ",
            " -",
            " Nested",
            " bullet",
            " under",
            " a",
            " loose",
            " item",
            "\n",
            "  ",
            " -",
            " Another",
            " nested",
            " bullet",
            "\n\n",
        ];

        // Simulate streaming with a frame after each delta.
        for d in deltas.iter() {
            ctrl.push(d);
            if let Some(cell) = ctrl.commit_for_frame() {
                lines.extend(cell.transcript_lines(u16::MAX));
            }
        }
        // Finalize and flush remaining lines now.
        if let Some(cell) = ctrl.finalize().0 {
            lines.extend(cell.transcript_lines(u16::MAX));
        }

        let streamed: Vec<_> = lines_to_plain_strings(&lines)
            .into_iter()
            // skip • and 2-space indentation
            .map(|s| s.chars().skip(2).collect::<String>())
            .collect();

        // Full render of the same source
        let source: String = deltas.iter().copied().collect();
        let mut rendered: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown(&source, None, &mut rendered);
        let rendered_strs = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, rendered_strs);

        // Also assert exact expected plain strings for clarity.
        let expected = vec![
            "Loose vs. tight list items:".to_string(),
            "".to_string(),
            "1. Tight item".to_string(),
            "2. Another tight item".to_string(),
            "3. Loose item with its own paragraph.".to_string(),
            "".to_string(),
            "   This paragraph belongs to the same list item.".to_string(),
            "4. Second loose item with a nested list after a blank line.".to_string(),
            "    - Nested bullet under a loose item".to_string(),
            "    - Another nested bullet".to_string(),
        ];
        assert_eq!(
            streamed, expected,
            "expected exact rendered lines for loose/tight section"
        );
    }

    #[test]
    fn finalize_returns_the_complete_raw_markdown_source() {
        let mut controller = StreamController::new(Some(20));
        controller.push("A **formatted** line\n");
        let _ = controller.commit_for_frame();
        controller.push("and a final fragment");

        let (_cell, source) = controller.finalize();

        assert_eq!(
            source.as_deref(),
            Some("A **formatted** line\nand a final fragment")
        );
    }
}
