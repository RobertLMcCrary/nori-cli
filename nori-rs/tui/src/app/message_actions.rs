//! Carrying out what the user chose for a message of the transcript overlay.

use std::sync::Arc;

use super::App;
use crate::app_event::AppEvent;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::pager_overlay::MessageAction;
use crate::pager_overlay::MessageChoice;
use crate::tui;

impl App {
    pub(crate) fn on_message_action(&mut self, tui: &mut tui::Tui, action: MessageAction) {
        self.close_transcript_overlay(tui);
        tui.frame_requester().schedule_frame();
        if self.chat_widget.is_task_running() {
            self.chat_widget.add_info_message(
                "A turn is running: stop it with Esc before forking or rewinding.".to_owned(),
                None,
            );
            return;
        }
        let prompt_at = |cell: usize| {
            self.transcript_cells
                .get(cell)
                .and_then(|cell| cell.as_any().downcast_ref::<UserHistoryCell>())
                .map(|user| user.message.clone())
        };
        match action.choice {
            MessageChoice::EditPrompt => {
                let prefill = prompt_at(action.cell).unwrap_or_default();
                self.app_event_tx.send(AppEvent::ForkToMessage {
                    cell_index: action.cell,
                    prefill,
                });
            }
            MessageChoice::ForkThrough => {
                let cut = end_of_message(&self.transcript_cells, action.cell);
                // Nothing said after it: the agent's own fork keeps everything, tool calls
                // included, where a fork before a message has only a summary to go on.
                let at_head = self.transcript_cells[cut..].iter().all(|cell| {
                    !cell.as_any().is::<UserHistoryCell>()
                        && !cell.as_any().is::<AgentMessageCell>()
                });
                if at_head && self.chat_widget.agent_capabilities().session_fork {
                    self.app_event_tx.send(AppEvent::BranchFromCurrent);
                } else {
                    self.app_event_tx.send(AppEvent::ForkToMessage {
                        cell_index: cut,
                        prefill: String::new(),
                    });
                }
            }
            MessageChoice::Rewind => {
                let prompts = (action.cell..self.transcript_cells.len())
                    .filter_map(prompt_at)
                    .collect();
                let prompt = prompt_at(action.cell).unwrap_or_default();
                self.chat_widget.begin_rewind(crate::chatwidget::Rewind {
                    cell_index: action.cell,
                    prompt,
                    prompts,
                });
            }
        }
    }
}

/// Where the message at `cell` ends: after it and the cells that continue it.
fn end_of_message(cells: &[Arc<dyn HistoryCell>], cell: usize) -> usize {
    let mut end = cell + 1;
    while cells
        .get(end)
        .is_some_and(|next| next.is_stream_continuation())
    {
        end += 1;
    }
    end.min(cells.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::text::Line;

    #[test]
    fn a_message_ends_after_the_cells_that_continue_it() {
        let cells: Vec<Arc<dyn HistoryCell>> = vec![
            Arc::new(UserHistoryCell {
                message: "question".to_owned(),
            }),
            Arc::new(AgentMessageCell::new(vec![Line::from("first")], true)),
            Arc::new(AgentMessageCell::new(vec![Line::from("more")], false)),
            Arc::new(UserHistoryCell {
                message: "next".to_owned(),
            }),
        ];
        assert_eq!(end_of_message(&cells, 1), 3);
        assert_eq!(end_of_message(&cells, 0), 1);
        assert_eq!(end_of_message(&cells, 3), 4);
    }
}
