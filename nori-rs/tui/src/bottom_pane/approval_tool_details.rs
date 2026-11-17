//! Reuse conversation tool details for the debug permission bridge.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use crate::client_tool_cell::ClientToolCell;
use crate::render::renderable::Renderable;

pub(super) struct ToolDetails(pub(super) ClientToolCell);

impl Renderable for ToolDetails {
    fn render(&self, area: Rect, buffer: &mut Buffer) {
        Paragraph::new(self.0.expanded_lines(area.width)).render(area, buffer);
    }

    fn desired_height(&self, width: u16) -> u16 {
        u16::try_from(self.0.expanded_lines(width).len()).unwrap_or(u16::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::ToolKind;
    use crate::presentation::ToolPhase;
    use crate::presentation::ToolSnapshot;

    #[test]
    fn permission_details_use_the_shared_semantic_read_view() {
        let details = ToolDetails(ClientToolCell::new(
            ToolSnapshot {
                call_id: "read-config".into(),
                title: "Read configuration".into(),
                kind: ToolKind::Read,
                phase: ToolPhase::PendingApproval,
                locations: Vec::new(),
                invocation: None,
                artifacts: Vec::new(),
                raw_input: Some(serde_json::json!({"path": "/etc/config.toml"})),
                raw_output: Some(serde_json::json!({"success": true, "lines": 42})),
                owner_request_id: None,
            },
            "/".into(),
            false,
        ));
        let area = Rect::new(0, 0, 40, details.desired_height(40));
        let mut buffer = Buffer::empty(area);
        details.render(area, &mut buffer);
        let display = (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(display.contains("/etc/config.toml"));
        assert!(display.contains("42 lines read"));
        assert!(!display.contains("\"path\"") && !display.contains('{'));
        insta::assert_snapshot!("shared_read_details", display);
    }
}
