//! The live block: one header and one activity row per running subagent.

use std::time::Duration;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use super::SubagentTree;
use super::cell::format_elapsed;
use super::cell::tool_count_text;
use super::node::Node;
use crate::render::renderable::Renderable;
use crate::text_formatting::truncate_text;

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_PERIOD: Duration = Duration::from_millis(80);
/// The block never takes more than this fraction of the screen, and never fewer rows than this.
const SCREEN_FRACTION: u16 = 3;
const MIN_ROWS: u16 = 4;

/// The rows built for one node, valid while the node, the width, its depth and the elapsed
/// seconds stay the same.
#[derive(Debug, Clone)]
pub(super) struct RowCache {
    key: (u16, usize, i64, u64),
    /// Header after the spinner, then the activity row.
    header: Vec<Span<'static>>,
    activity: Line<'static>,
}

pub(crate) struct SubagentBlock<'a> {
    pub(crate) tree: &'a SubagentTree,
    pub(crate) now: Instant,
    /// Terminal rows, or zero when unknown (no cap).
    pub(crate) screen_rows: u16,
}

impl SubagentBlock<'_> {
    fn spinner(&self, node: &Node) -> Span<'static> {
        if !self.tree.animations() {
            return "•".dim();
        }
        let phase = self
            .now
            .saturating_duration_since(node.started_at)
            .as_millis()
            / SPINNER_PERIOD.as_millis().max(1);
        SPINNER_FRAMES[usize::try_from(phase % 10).unwrap_or(0)].cyan()
    }

    fn rows_for(
        &self,
        node: &Node,
        depth: usize,
        width: u16,
    ) -> (Vec<Span<'static>>, Line<'static>) {
        let elapsed = self.now.saturating_duration_since(node.started_at);
        let key = (width, depth, node.revision, elapsed.as_secs());
        if let Some(cache) = node.row_cache.borrow().as_ref()
            && cache.key == key
        {
            return (cache.header.clone(), cache.activity.clone());
        }
        self.tree.rows_built.set(self.tree.rows_built.get() + 1);
        let indent = "  ".repeat(depth);
        let usable = usize::from(width).saturating_sub(indent.len() + 2).max(8);
        let trailer = match node.tool_count {
            0 => format!(" · {}", format_elapsed(elapsed)),
            count => format!(
                " · {} · {}",
                tool_count_text(count),
                format_elapsed(elapsed)
            ),
        };
        let kind = node
            .kind()
            .map(|kind| format!("  {kind}"))
            .unwrap_or_default();
        let label_room = usable.saturating_sub(trailer.chars().count() + kind.chars().count());
        let label = truncate_text(node.label(), label_room.max(6));
        let mut header: Vec<Span<'static>> =
            vec![indent.clone().into(), format!(" {label}").bold()];
        if usable > label.chars().count() + kind.chars().count() + trailer.chars().count() {
            header.push(kind.dim());
        }
        header.push(trailer.dim());
        let activity_text = if node.activity.is_empty() {
            "starting…".to_owned()
        } else {
            truncate_text(&node.activity, usable)
        };
        let activity = Line::from(vec![format!("{indent}  ").into(), activity_text.dim()]);
        *node.row_cache.borrow_mut() = Some(RowCache {
            key,
            header: header.clone(),
            activity: activity.clone(),
        });
        (header, activity)
    }

    /// The rows to show at `width`: whole nodes while they fit, then a count of the rest.
    ///
    /// Only the nodes shown are visited, so a frame costs the rows on screen however many
    /// subagents run.
    pub(crate) fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let total = self.tree.running_count();
        if total == 0 {
            return Vec::new();
        }
        let budget = if self.screen_rows == 0 {
            usize::MAX
        } else {
            usize::from((self.screen_rows / SCREEN_FRACTION).max(MIN_ROWS))
        };
        // Two rows per node; when they do not all fit, one row is kept for the count.
        let limit = if total.saturating_mul(2) <= budget {
            total
        } else {
            budget.saturating_sub(1) / 2
        };
        let shown = self.tree.display_order(limit);
        let mut lines = Vec::with_capacity(shown.len() * 2 + 1);
        for (node, depth) in &shown {
            let (mut header, activity) = self.rows_for(node, *depth, width);
            // The spinner goes after the indent.
            header.insert(1, self.spinner(node));
            lines.push(Line::from(header));
            lines.push(activity);
        }
        if shown.len() < total {
            lines.push(Line::from(format!("  +{} more running", total - shown.len())).dim());
        }
        lines
    }
}

impl Renderable for SubagentBlock<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(Text::from(self.lines(area.width))).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        u16::try_from(self.lines(width).len()).unwrap_or(u16::MAX)
    }
}
