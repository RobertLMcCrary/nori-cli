//! Collecting the last rows of a conversation.

use ratatui::text::Line;

use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::pager_overlay::transcript_overlay_lines;

/// The rows of one cell of a conversation, as the transcript overlay shows them except that what
/// an agent said is shown whole: its end is what a viewer follows, and the overlay's cut would
/// drop exactly that.
pub(crate) fn cell_lines(cell: &dyn HistoryCell, width: u16) -> Vec<Line<'static>> {
    let any = cell.as_any();
    if any.is::<AgentMarkdownCell>() || any.is::<AgentMessageCell>() {
        cell.transcript_lines(width)
    } else {
        transcript_overlay_lines(cell, width)
    }
}

/// The last rows of a conversation, collected from its end so the cost is the rows kept and
/// not the length of what came before.
pub(crate) struct Tail {
    rows: usize,
    /// Chunks from the newest to the oldest.
    chunks: Vec<Vec<Line<'static>>>,
    total: usize,
}

impl Tail {
    pub(crate) fn new(rows: usize) -> Self {
        Self {
            rows,
            chunks: Vec::new(),
            total: 0,
        }
    }

    pub(crate) fn is_full(&self) -> bool {
        self.total >= self.rows
    }

    /// Add what came before everything collected so far.
    pub(crate) fn push_front(&mut self, lines: Vec<Line<'static>>) {
        if lines.is_empty() {
            return;
        }
        // One blank row between parts, as the transcript overlay puts between cells.
        self.total += lines.len() + usize::from(!self.chunks.is_empty());
        self.chunks.push(lines);
    }

    pub(crate) fn finish(self) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(self.total);
        for (index, chunk) in self.chunks.into_iter().rev().enumerate() {
            if index > 0 {
                lines.push(Line::default());
            }
            lines.extend(chunk);
        }
        let excess = lines.len().saturating_sub(self.rows);
        lines.drain(..excess);
        lines
    }
}
