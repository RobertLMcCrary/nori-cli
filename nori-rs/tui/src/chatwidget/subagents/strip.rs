//! Single-row, horizontally scrollable subagent pills.

use std::cell::Cell;
use std::cell::RefCell;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::SubagentTree;
use crate::render::renderable::Renderable;
use crate::subagent_switcher::Status;
use crate::subagent_switcher::SubagentKey;
use crate::subagent_switcher::Target;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StripAction {
    Focus(SubagentKey),
    Previous,
    Next,
}

#[derive(Default)]
pub(crate) struct StripState {
    offset: Cell<usize>,
    selected: Cell<Option<SubagentKey>>,
    manual_scroll: Cell<bool>,
    hits: RefCell<Vec<(Rect, StripAction)>>,
    area: Cell<Option<Rect>>,
}

impl StripState {
    #[cfg(test)]
    pub(crate) fn area(&self) -> Option<Rect> {
        self.area.get()
    }
    pub(crate) fn action_at(&self, column: u16, row: u16) -> Option<StripAction> {
        self.hits
            .borrow()
            .iter()
            .find(|(area, _)| area.contains(ratatui::layout::Position::new(column, row)))
            .map(|(_, action)| *action)
    }

    pub(crate) fn contains(&self, column: u16, row: u16) -> bool {
        self.area
            .get()
            .is_some_and(|area| area.contains(ratatui::layout::Position::new(column, row)))
    }

    pub(crate) fn scroll(&self, delta: isize) {
        self.offset
            .set(self.offset.get().saturating_add_signed(delta));
        self.manual_scroll.set(true);
    }
}

pub(crate) struct SubagentStrip<'a> {
    pub(crate) tree: &'a SubagentTree,
    pub(crate) focused: Option<SubagentKey>,
    pub(crate) state: &'a StripState,
}

impl Renderable for SubagentStrip<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.state.area.set(Some(area));
        let mut hits = self.state.hits.borrow_mut();
        let focused_was_visible = self.focused.is_some_and(|key| {
            hits.iter()
                .any(|(_, action)| *action == StripAction::Focus(key))
        });
        hits.clear();
        if area.width < 3 || area.height == 0 {
            return;
        }
        let (mut entries, _) = self.tree.entries(Instant::now());
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.active_at));
        if entries.is_empty() {
            return;
        }
        let inner = area.width.saturating_sub(2);
        let card_width = if inner >= 20 {
            (inner / 2).min(18)
        } else {
            inner
        };
        if card_width < 3 {
            return;
        }
        let count = usize::from((inner / card_width).max(1));
        let max_offset = entries.len().saturating_sub(count);
        let mut offset = self.state.offset.get().min(max_offset);
        let focus_changed = self.focused != self.state.selected.replace(self.focused);
        let manual_scroll = self.state.manual_scroll.replace(false);
        if (focus_changed || (focused_was_visible && !manual_scroll))
            && let Some(focused) = self.focused
            && let Some(index) = entries
                .iter()
                .position(|entry| entry.target == Target::Subagent(focused))
        {
            if index < offset {
                offset = index;
            }
            if index >= offset + count {
                offset = index + 1 - count;
            }
        }
        self.state.offset.set(offset);
        if offset > 0 {
            buf.set_string(area.x, area.y, "‹", "‹".cyan().style);
            hits.push((Rect::new(area.x, area.y, 1, 1), StripAction::Previous));
        }
        if offset + count < entries.len() {
            buf.set_string(area.right() - 1, area.y, "›", "›".cyan().style);
            hits.push((Rect::new(area.right() - 1, area.y, 1, 1), StripAction::Next));
        }
        for (slot, entry) in entries.iter().skip(offset).take(count).enumerate() {
            let Target::Subagent(key) = entry.target else {
                continue;
            };
            let x = area.x + 1 + u16::try_from(slot).unwrap_or(u16::MAX) * card_width;
            let selected = self.focused == Some(key);
            let icon = match entry.status {
                Status::Running => "●",
                Status::Idle => "○",
                Status::Completed => "✓",
                Status::Failed => "×",
                Status::Cancelled => "−",
            };
            let title_width = usize::from(card_width).saturating_sub(5);
            let mut width = 0;
            let title: String = entry
                .title
                .graphemes(true)
                .take_while(|grapheme| {
                    let next = width + UnicodeWidthStr::width(*grapheme);
                    if next > title_width {
                        return false;
                    }
                    width = next;
                    true
                })
                .collect();
            let label = format!("│{icon} {title}");
            let fill =
                usize::from(card_width).saturating_sub(UnicodeWidthStr::width(label.as_str()) + 1);
            let pill = format!("{label}{}│", " ".repeat(fill));
            let style = if selected {
                "".cyan().bold().style
            } else {
                match entry.status {
                    Status::Running => "".cyan().style,
                    Status::Completed => "".green().style,
                    Status::Failed => "".red().style,
                    Status::Idle | Status::Cancelled => "".dim().style,
                }
            };
            buf.set_stringn(x, area.y, pill, usize::from(card_width), style);
            hits.push((Rect::new(x, area.y, card_width, 1), StripAction::Focus(key)));
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        if self.tree.visible_count() == 0 { 0 } else { 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nori_protocol::Patch;
    use nori_protocol::SubagentState;
    use nori_protocol::SubagentUpdate;
    use nori_protocol::acp::v1 as acp;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;
    use std::time::Duration;

    fn tree() -> SubagentTree {
        let now = Instant::now();
        let root = acp::SessionId::new("root");
        let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
        tree.set_root(root.clone());
        for (index, label) in ["Research", "Implement", "Review", "Verify"]
            .into_iter()
            .enumerate()
        {
            tree.on_update(
                SubagentUpdate {
                    parent_session_id: root.clone(),
                    session_id: acp::SessionId::new(format!("child-{index}")),
                    agent: Patch::Keep,
                    title: Patch::Set(label.to_owned()),
                    description: Patch::Keep,
                    state: Patch::Set(SubagentState::Idle),
                },
                now + Duration::from_secs(u64::try_from(index).unwrap()),
            );
        }
        tree
    }

    fn render(
        tree: &SubagentTree,
        state: &StripState,
        width: u16,
        focused: Option<SubagentKey>,
    ) -> String {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        SubagentStrip {
            tree,
            focused,
            state,
        }
        .render(area, &mut buf);
        (0..width)
            .map(|x| buf[(x, 0)].symbol().chars().next().unwrap_or(' '))
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn narrow_pills_scroll_and_keep_single_row_hit_targets() {
        let tree = tree();
        let state = StripState::default();
        insta::assert_snapshot!("horizontal_strip_narrow", render(&tree, &state, 30, None));
        assert_eq!(
            state.action_at(1, 0),
            Some(StripAction::Focus(SubagentKey(4)))
        );
        assert_eq!(state.action_at(1, 1), None);
        assert_eq!(state.action_at(29, 0), Some(StripAction::Next));
        state.scroll(1);
        let _ = render(&tree, &state, 30, None);
        assert_eq!(state.action_at(0, 0), Some(StripAction::Previous));
        assert_eq!(
            state.action_at(1, 0),
            Some(StripAction::Focus(SubagentKey(3)))
        );
        let _ = render(&tree, &state, 30, Some(SubagentKey(1)));
        assert_eq!(
            state.action_at(15, 0),
            Some(StripAction::Focus(SubagentKey(1)))
        );
    }

    #[test]
    fn wide_pills_remain_one_row_and_show_all_children() {
        let tree = tree();
        let state = StripState::default();
        insta::assert_snapshot!("horizontal_strip_wide", render(&tree, &state, 80, None));
    }

    #[test]
    fn status_updates_do_not_outrank_actual_work() {
        let now = Instant::now();
        let mut tree = tree();
        let state = StripState::default();
        let session = acp::SessionId::new("child-0");
        tree.on_child_update(
            super::super::ChildTarget::Session(&session),
            &acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                acp::ContentBlock::Text(acp::TextContent::new("working")),
            )),
            now + Duration::from_secs(30),
        );
        tree.on_update(
            SubagentUpdate {
                parent_session_id: acp::SessionId::new("root"),
                session_id: acp::SessionId::new("child-1"),
                agent: Patch::Keep,
                title: Patch::Keep,
                description: Patch::Keep,
                state: Patch::Set(SubagentState::Running),
            },
            now + Duration::from_secs(60),
        );
        let _ = render(&tree, &state, 30, None);
        assert_eq!(
            state.action_at(1, 0),
            Some(StripAction::Focus(SubagentKey(1)))
        );
    }
}
