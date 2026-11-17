//! Transcript content for the session focused in the shared chat view.

use std::cell::Cell;

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use crate::render::renderable::Renderable;

pub(crate) struct SessionTranscript {
    pub(crate) lines: Vec<Line<'static>>,
    /// Number of rows to move up from the latest output.
    pub(crate) scroll_offset: usize,
    focus_header: Option<usize>,
    viewport: Cell<Option<(Rect, usize)>>,
    tools: Vec<super::tool_projection::ToolSpan>,
    follow_live: bool,
}

impl SessionTranscript {
    pub(crate) fn new(lines: Vec<Line<'static>>, scroll_offset: usize) -> Self {
        Self {
            lines,
            scroll_offset,
            focus_header: None,
            viewport: Cell::new(None),
            tools: Vec::new(),
            follow_live: true,
        }
    }

    pub(crate) fn viewport_height(&self) -> Option<u16> {
        self.viewport.get().map(|(area, _)| area.height)
    }

    pub(crate) fn set_focus_header(&mut self, header: usize) {
        self.focus_header = Some(header);
    }

    pub(crate) fn set_tools(&mut self, tools: Vec<super::tool_projection::ToolSpan>) {
        self.tools = tools;
    }

    pub(crate) fn set_follow_live(&mut self, follow: bool) {
        self.follow_live = follow;
    }

    pub(crate) fn follows_live(&self) -> bool {
        self.follow_live
    }

    pub(crate) fn tool_at(&self, column: u16, row: u16) -> Option<String> {
        let (area, first) = self.viewport.get()?;
        if !area.contains(ratatui::layout::Position::new(column, row)) {
            return None;
        }
        let line = first + usize::from(row - area.y);
        self.tools
            .iter()
            .find(|tool| (tool.start..tool.end).contains(&line))
            .map(|tool| tool.call_id.clone())
    }

    pub(crate) fn nearest_tool(&self) -> Option<String> {
        let (area, first) = self.viewport.get()?;
        self.tools
            .iter()
            .rev()
            .find(|tool| tool.start < first + usize::from(area.height) && tool.end > first)
            .or_else(|| self.tools.last())
            .map(|tool| tool.call_id.clone())
    }

    pub(crate) fn action_at(&self, column: u16, row: u16, sent_prompt: bool) -> Option<KeyEvent> {
        let (area, first) = self.viewport.get()?;
        let header = self.focus_header?;
        if column < area.x
            || column >= area.right()
            || row < area.y
            || row >= area.bottom()
            || first + usize::from(row - area.y) != header
        {
            return None;
        }
        super::response_focus::action_at(column - area.x, area.width, sent_prompt)
    }
}

impl Renderable for SessionTranscript {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let content_height = self.lines.len();
        let first = content_height
            .saturating_sub(usize::from(area.height))
            .saturating_sub(self.scroll_offset);
        self.viewport.set(Some((area, first)));
        let end = first
            .saturating_add(usize::from(area.height))
            .min(content_height);
        Paragraph::new(Text::from(self.lines[first..end].to_vec())).render(area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        u16::try_from(self.lines.len()).unwrap_or(u16::MAX)
    }

    fn fills_available_height(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;
    use pretty_assertions::assert_eq;

    #[test]
    fn focus_hits_follow_the_scrolled_viewport_and_ignore_clipped_header() {
        let mut transcript =
            SessionTranscript::new((0..12).map(|_| Line::from("row")).collect(), 2);
        transcript.set_focus_header(7);
        let area = Rect::new(4, 5, 20, 4);
        transcript.render(area, &mut Buffer::empty(area));
        assert_eq!(
            transcript.action_at(7, 6, false),
            Some(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL))
        );
        assert_eq!(transcript.action_at(7, 5, false), None);
        assert_eq!(transcript.action_at(3, 6, false), None);
        transcript.scroll_offset = 0;
        transcript.render(area, &mut Buffer::empty(area));
        assert_eq!(transcript.action_at(7, 6, false), None);
    }

    #[test]
    fn tool_hits_follow_viewport_scroll_and_clipping() {
        let mut transcript =
            SessionTranscript::new((0..12).map(|_| Line::from("row")).collect(), 2);
        transcript.set_tools(vec![super::super::tool_projection::ToolSpan {
            start: 7,
            end: 9,
            call_id: "tool-1".to_owned(),
        }]);
        let area = Rect::new(4, 5, 20, 4);
        transcript.render(area, &mut Buffer::empty(area));
        assert_eq!(transcript.tool_at(7, 6), Some("tool-1".to_owned()));
        assert_eq!(transcript.tool_at(7, 5), None);
        assert_eq!(transcript.tool_at(3, 6), None);
        transcript.scroll_offset = 0;
        transcript.render(area, &mut Buffer::empty(area));
        assert_eq!(transcript.tool_at(7, 6), None);
    }
}
