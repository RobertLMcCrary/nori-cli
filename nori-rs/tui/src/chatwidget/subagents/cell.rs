use std::time::Duration;

use nori_protocol::SubagentOutcome;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;

use super::FinishedSubagent;
use crate::history_cell::HistoryCell;

/// Rows of the child's final message shown in the scrollback summary.
const SUMMARY_MESSAGE_ROWS: usize = 3;

/// One finished subagent: a compact summary for the scrollback, the whole detail for the
/// transcript overlay.
#[derive(Debug)]
pub(crate) struct SubagentSummaryCell {
    finished: FinishedSubagent,
}

impl SubagentSummaryCell {
    pub(crate) fn new(finished: FinishedSubagent) -> Self {
        Self { finished }
    }

    fn headline(&self) -> Line<'static> {
        let finished = &self.finished;
        let (mark, detail) = match &finished.outcome {
            SubagentOutcome::Completed { .. } => ("✓".green(), None),
            SubagentOutcome::Failed { reason } => ("✗".red(), reason.clone()),
            SubagentOutcome::Cancelled => ("⊘".dim(), Some("cancelled".to_owned())),
        };
        let mut spans: Vec<Span<'static>> = vec![
            "  ".repeat(finished.depth).into(),
            mark,
            " ".into(),
            finished.label.clone().bold(),
        ];
        if let Some(kind) = &finished.kind {
            spans.push(format!("  {kind}").dim());
        }
        spans.push(
            match finished.tool_count {
                0 => format!(" · {}", format_elapsed(finished.elapsed)),
                count => format!(
                    " · {} · {}",
                    tool_count_text(count),
                    format_elapsed(finished.elapsed)
                ),
            }
            .dim(),
        );
        if let Some(detail) = detail {
            spans.push(format!(" · {detail}").dim());
        }
        Line::from(spans)
    }
}

impl HistoryCell for SubagentSummaryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = vec![self.headline()];
        let indent = "  ".repeat(self.finished.depth + 1);
        let message = self.finished.message.trim();
        if !message.is_empty() {
            // The child's answer is markdown; its first rendered rows stand for it, dimmed so the
            // root conversation stays the brightest thing on screen.
            let rendered: Vec<Line<'static>> =
                crate::markdown_render::render_markdown_text_with_width(
                    message,
                    Some(usize::from(width).saturating_sub(indent.len()).max(1)),
                )
                .lines
                .into_iter()
                .filter(|line| {
                    line.spans
                        .iter()
                        .any(|span| !span.content.trim().is_empty())
                })
                .collect();
            let total = rendered.len();
            for line in rendered.into_iter().take(SUMMARY_MESSAGE_ROWS) {
                let mut spans = vec![Span::from(indent.clone())];
                spans.extend(line.spans.into_iter().map(ratatui::prelude::Stylize::dim));
                lines.push(Line::from(spans));
            }
            if total > SUMMARY_MESSAGE_ROWS {
                lines.push(Line::from(vec![
                    indent.into(),
                    format!(
                        "… {} more lines (ctrl+t for the full transcript)",
                        total - SUMMARY_MESSAGE_ROWS
                    )
                    .dim(),
                ]));
            }
        }
        lines
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = vec![self.headline()];
        for cell in &self.finished.detail {
            for mut line in cell.transcript_lines(width.saturating_sub(4).max(1)) {
                line.spans.insert(0, "    ".into());
                lines.push(line);
            }
        }
        lines
    }

    fn full_transcript(&self) -> bool {
        true
    }
}

pub(super) fn tool_count_text(count: usize) -> String {
    match count {
        1 => "1 tool".to_owned(),
        count => format!("{count} tools"),
    }
}

/// `7s`, `1m 05s`.
pub(super) fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn finished(outcome: SubagentOutcome, message: &str) -> FinishedSubagent {
        FinishedSubagent {
            label: "Read a.txt contents".into(),
            kind: Some("general-purpose".into()),
            outcome,
            tool_count: 1,
            elapsed: Duration::from_secs(34),
            message: message.into(),
            detail: Vec::new(),
            depth: 0,
        }
    }

    #[test]
    fn completed_child_summary() {
        let cell = SubagentSummaryCell::new(finished(
            SubagentOutcome::Completed {
                stop_reason: Some("end_turn".into()),
            },
            "The file holds alpha.",
        ));
        assert_eq!(
            plain(&cell.display_lines(80)),
            vec![
                "✓ Read a.txt contents  general-purpose · 1 tool · 34s".to_string(),
                "  The file holds alpha.".to_string(),
            ]
        );
    }

    #[test]
    fn failed_and_cancelled_children() {
        let failed = SubagentSummaryCell::new(finished(
            SubagentOutcome::Failed {
                reason: Some("usage limit".into()),
            },
            "",
        ));
        assert_eq!(
            plain(&failed.display_lines(80)),
            vec!["✗ Read a.txt contents  general-purpose · 1 tool · 34s · usage limit".to_string()]
        );
        let cancelled = SubagentSummaryCell::new(finished(SubagentOutcome::Cancelled, ""));
        assert!(plain(&cancelled.display_lines(80))[0].contains("cancelled"));
    }

    #[test]
    fn long_messages_are_collapsed() {
        let message = (0..10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cell = SubagentSummaryCell::new(finished(
            SubagentOutcome::Completed { stop_reason: None },
            &message,
        ));
        let lines = plain(&cell.display_lines(80));
        assert_eq!(lines.len(), 1 + SUMMARY_MESSAGE_ROWS + 1);
        assert!(lines.last().is_some_and(|line| line.contains("more lines")));
    }

    #[test]
    fn the_message_is_rendered_as_markdown() {
        let cell = SubagentSummaryCell::new(finished(
            SubagentOutcome::Completed { stop_reason: None },
            "**Done.** The files:\n\n- `a.txt` holds alpha\n- `b.txt` holds beta",
        ));
        assert_eq!(
            plain(&cell.display_lines(80))[1..],
            [
                "  Done. The files:".to_string(),
                "  - a.txt holds alpha".to_string(),
                "  - b.txt holds beta".to_string(),
            ]
        );
    }

    #[test]
    fn elapsed_formats() {
        assert_eq!(format_elapsed(Duration::from_secs(7)), "7s");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m 05s");
    }
}
