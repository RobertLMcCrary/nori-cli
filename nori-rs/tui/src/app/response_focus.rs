//! Inline navigation of sent user messages in the focused conversation.

use std::sync::Arc;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::style::Stylize;
use ratatui::text::Line;

use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::pager_overlay::MessageAction;
use crate::pager_overlay::MessageChoice;
use crate::subagent_switcher::Target;
use crate::tui;

pub(crate) struct ResponseFocus {
    target: Target,
    cells: Vec<Arc<dyn HistoryCell>>,
    selected: usize,
}

impl ResponseFocus {
    fn navigate(&mut self, key: KeyEvent) -> Option<bool> {
        match (key.code, key.modifiers) {
            (KeyCode::Char('h'), KeyModifiers::CONTROL) => Some(self.step(true)),
            (KeyCode::Char('l'), KeyModifiers::CONTROL) => Some(self.step(false)),
            _ => None,
        }
    }

    fn step(&mut self, older: bool) -> bool {
        let next = adjacent_prompt(&self.cells, self.selected, older);
        if let Some(next) = next {
            self.selected = next;
            true
        } else {
            false
        }
    }

    pub(super) fn rendered(&self, width: u16) -> (Vec<Line<'static>>, usize, usize) {
        let mut lines = Vec::new();
        let mut selected_end = 0;
        let mut selected = false;
        let mut header = 0;
        for (index, cell) in self.cells.iter().enumerate() {
            if index > 0 && !cell.is_stream_continuation() {
                lines.push(Line::default());
            }
            if index == self.selected {
                selected = true;
                header = lines.len();
                lines.push(crate::chatwidget::response_focus::header(width, false));
            } else if !cell.is_stream_continuation() {
                selected = false;
            }
            let rendered = cell.display_lines(width);
            if selected {
                lines.extend(rendered.into_iter().map(Stylize::reversed));
                selected_end = lines.len();
            } else {
                lines.extend(rendered);
            }
        }
        let offset = lines.len().saturating_sub(selected_end);
        (lines, offset, header)
    }
}

impl super::App {
    pub(super) fn begin_response_focus(
        &mut self,
        tui: &mut tui::Tui,
        cells: Vec<Arc<dyn HistoryCell>>,
    ) {
        let Some(selected) = cells.iter().rposition(is_prompt) else {
            self.chat_widget
                .add_info_message("No sent message is available to select.".to_owned(), None);
            return;
        };
        self.chat_widget.set_response_focus(true);
        self.backtrack.response_focus = Some(ResponseFocus {
            target: self.session_view.focused(),
            cells,
            selected,
        });
        let width = tui.terminal.last_known_screen_size.width;
        let height = self
            .chat_widget
            .response_focus_viewport_height(tui.terminal.last_known_screen_size.height);
        let offset = self
            .backtrack
            .response_focus
            .as_ref()
            .map(|focus| {
                let (lines, offset, header) = focus.rendered(width);
                offset.max(lines.len().saturating_sub(header.saturating_add(height)))
            })
            .unwrap_or(0);
        self.focus_response_view(tui, offset);
        tui.frame_requester().schedule_frame();
    }

    pub(super) fn handle_response_focus(&mut self, tui: &mut tui::Tui, key: KeyEvent) -> bool {
        let Some(focus) = self.backtrack.response_focus.as_mut() else {
            return false;
        };
        if focus.target != self.session_view.focused() {
            self.backtrack.response_focus = None;
            self.chat_widget.set_response_focus(false);
            self.end_response_view(tui);
            return false;
        }
        if key.kind != KeyEventKind::Press {
            return true;
        }
        let moved = match (key.code, key.modifiers) {
            (KeyCode::Char('h' | 'l'), KeyModifiers::CONTROL) => {
                let moved = focus.navigate(key).unwrap_or(false);
                if !moved && key.code == KeyCode::Char('l') {
                    self.backtrack.response_focus = None;
                    self.chat_widget.set_response_focus(false);
                    self.end_response_view(tui);
                    tui.frame_requester().schedule_frame();
                    return true;
                }
                moved
            }
            (KeyCode::Enter, KeyModifiers::NONE) | (KeyCode::Char(' '), KeyModifiers::NONE) => {
                let Some(response) = response_for_prompt(&focus.cells, focus.selected) else {
                    self.chat_widget.add_info_message(
                        "This sent message has no completed response to select.".to_owned(),
                        None,
                    );
                    return true;
                };
                let cell = if focus.target == Target::Main {
                    self.transcript_cells
                        .iter()
                        .position(|cell| Arc::ptr_eq(cell, &focus.cells[response]))
                } else {
                    Some(response)
                };
                let Some(cell) = cell else {
                    self.backtrack.response_focus = None;
                    self.chat_widget.set_response_focus(false);
                    self.end_response_view(tui);
                    self.chat_widget.add_error_message(
                        "The selected response is no longer available.".to_owned(),
                    );
                    return true;
                };
                let action = MessageAction {
                    cell,
                    choice: if key.code == KeyCode::Enter {
                        MessageChoice::RollbackThrough
                    } else {
                        MessageChoice::ForkSubagent
                    },
                };
                self.backtrack.response_focus = None;
                self.chat_widget.set_response_focus(false);
                self.end_response_view(tui);
                self.on_message_action(tui, action);
                return true;
            }
            (KeyCode::Esc, _) => {
                self.backtrack.response_focus = None;
                self.chat_widget.set_response_focus(false);
                self.end_response_view(tui);
                tui.frame_requester().schedule_frame();
                return true;
            }
            _ => return true,
        };
        if moved {
            self.chat_widget.set_response_focus_prompt(false);
            let width = tui.terminal.last_known_screen_size.width;
            let height = self
                .chat_widget
                .response_focus_viewport_height(tui.terminal.last_known_screen_size.height);
            let (lines, offset, header) = focus.rendered(width);
            let offset = offset.max(lines.len().saturating_sub(header.saturating_add(height)));
            self.focus_response_view(tui, offset);
            tui.frame_requester().schedule_frame();
        }
        true
    }
}

fn is_response(cell: &Arc<dyn HistoryCell>) -> bool {
    (cell.as_any().is::<AgentMessageCell>() && !cell.is_stream_continuation())
        || cell.as_any().is::<AgentMarkdownCell>()
}

fn is_prompt(cell: &Arc<dyn HistoryCell>) -> bool {
    cell.as_any().is::<UserHistoryCell>()
}

fn adjacent_prompt(cells: &[Arc<dyn HistoryCell>], current: usize, older: bool) -> Option<usize> {
    if older {
        cells[..current].iter().rposition(is_prompt)
    } else {
        cells
            .iter()
            .enumerate()
            .skip(current + 1)
            .find_map(|(index, cell)| is_prompt(cell).then_some(index))
    }
}

fn response_for_prompt(cells: &[Arc<dyn HistoryCell>], prompt: usize) -> Option<usize> {
    let start = prompt.checked_add(1)?;
    let tail = cells.get(start..)?;
    let end = tail.iter().position(is_prompt).unwrap_or(tail.len());
    tail[..end]
        .iter()
        .rposition(is_response)
        .map(|index| start + index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::text::Line;

    fn history() -> Vec<Arc<dyn HistoryCell>> {
        vec![
            Arc::new(UserHistoryCell {
                message: "First question".to_owned(),
            }),
            Arc::new(AgentMessageCell::new(
                vec![Line::from("first answer")],
                true,
            )),
            Arc::new(AgentMessageCell::new(vec![Line::from("continued")], false)),
            Arc::new(UserHistoryCell {
                message: "Second question".to_owned(),
            }),
            Arc::new(AgentMessageCell::new(
                vec![Line::from("second answer")],
                true,
            )),
            Arc::new(UserHistoryCell {
                message: "Pending question".to_owned(),
            }),
        ]
    }

    #[test]
    fn navigation_visits_only_sent_messages_and_never_responses() {
        let cells = history();
        let mut focus = ResponseFocus {
            target: Target::Main,
            cells,
            selected: 5,
        };
        let previous = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
        let next = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(focus.navigate(previous), Some(true));
        assert_eq!(focus.selected, 3);
        assert_eq!(focus.navigate(previous), Some(true));
        assert_eq!(focus.selected, 0);
        assert_eq!(focus.navigate(previous), Some(false));
        assert_eq!(focus.navigate(next), Some(true));
        assert_eq!(focus.selected, 3);
        assert_eq!(focus.navigate(next), Some(true));
        assert_eq!(focus.selected, 5);
        assert_eq!(focus.navigate(next), Some(false));
    }

    #[test]
    fn user_message_actions_target_the_last_response_within_that_turn() {
        let cells = history();
        assert_eq!(response_for_prompt(&cells, 0), Some(1));
        assert_eq!(response_for_prompt(&cells, 3), Some(4));
        assert_eq!(response_for_prompt(&cells, 5), None);
    }

    #[test]
    fn focused_sent_message_headers_fit_narrow_and_wide_views() {
        let cells = history();
        let focus = ResponseFocus {
            target: Target::Main,
            cells,
            selected: 3,
        };
        for (name, width) in [
            ("focused_sent_message_narrow", 30),
            ("focused_sent_message_wide", 80),
        ] {
            let (lines, _, _) = focus.rendered(width);
            let text = lines
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            insta::assert_snapshot!(name, text);
        }
    }

    #[test]
    fn focus_highlights_the_user_message_and_keeps_the_full_conversation() {
        let focus = ResponseFocus {
            target: Target::Main,
            cells: history(),
            selected: 3,
        };
        let (lines, _, header) = focus.rendered(80);
        assert!(
            lines[header + 1]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("First question"));
        assert!(text.contains("second answer"));
        assert!(text.contains("Pending question"));
    }
}
