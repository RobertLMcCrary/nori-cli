//! Distinguish a tool tap from a swipe on its conversation row.

use crate::subagent_switcher::Target;

#[derive(Default)]
pub(super) struct ToolGesture {
    pending: Option<(Target, String, u16, u16, bool)>,
}

impl ToolGesture {
    pub(super) fn cancel(&mut self) {
        self.pending = None;
    }
    pub(super) fn press(&mut self, target: Target, call_id: String, column: u16, row: u16) {
        self.pending = Some((target, call_id, column, row, false));
    }

    pub(super) fn drag(&mut self, target: Target, column: u16, row: u16) {
        if let Some((original, _, x, y, cancelled)) = &mut self.pending
            && (*original != target || x.abs_diff(column) > 0 || y.abs_diff(row) > 0)
        {
            *cancelled = true;
        }
    }

    pub(super) fn release(&mut self, target: Target, hit: Option<&str>) -> Option<String> {
        let (original, call_id, _, _, cancelled) = self.pending.take()?;
        (!cancelled && original == target && hit == Some(call_id.as_str())).then_some(call_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn swipe_does_not_toggle_but_tap_on_same_tool_does() {
        let mut gesture = ToolGesture::default();
        let mut view = crate::app::session_view::SessionView::default();
        gesture.press(Target::Main, "tool".to_owned(), 5, 8);
        gesture.drag(Target::Main, 5, 10);
        view.scroll(2);
        assert_eq!(view.scroll_offset(), 2);
        assert_eq!(gesture.release(Target::Main, Some("tool")), None);
        gesture.press(Target::Main, "tool".to_owned(), 5, 8);
        assert_eq!(
            gesture.release(Target::Main, Some("tool")),
            Some("tool".to_owned())
        );
        gesture.press(Target::Main, "tool".to_owned(), 5, 8);
        assert_eq!(
            gesture.release(
                Target::Subagent(crate::subagent_switcher::SubagentKey(1)),
                Some("tool")
            ),
            None
        );
    }
}
