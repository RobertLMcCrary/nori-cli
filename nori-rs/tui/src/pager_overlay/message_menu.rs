//! The actions offered for one message of the transcript overlay.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

/// What the user can do with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageChoice {
    /// A new conversation with everything before this prompt, and the prompt in the composer.
    EditPrompt,
    /// Restore the files this prompt's turn and the turns after it changed, then edit it.
    Rewind,
    /// A new conversation with everything up to and including this message.
    ForkThrough,
    RollbackThrough,
    ForkSubagent,
}

/// A choice made for the message at `cell` of the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MessageAction {
    pub(crate) cell: usize,
    pub(crate) choice: MessageChoice,
}

impl MessageChoice {
    fn label(self) -> &'static str {
        match self {
            MessageChoice::EditPrompt => "Edit this prompt",
            MessageChoice::Rewind => "Rewind here and restore files",
            MessageChoice::ForkThrough => "Fork from here",
            MessageChoice::RollbackThrough => "Roll back here",
            MessageChoice::ForkSubagent => "Fork subagent",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            MessageChoice::EditPrompt => "New conversation before it, prompt in the composer",
            MessageChoice::Rewind => "Restores the files edited since, then edits it",
            MessageChoice::ForkThrough => "A new conversation with everything up to here",
            MessageChoice::RollbackThrough => "Continue from this response",
            MessageChoice::ForkSubagent => "Start a subagent from this response",
        }
    }
}

/// The open menu of one message.
pub(super) struct MessageMenu {
    cell: usize,
    choices: Vec<MessageChoice>,
    selected: usize,
}

/// What a key did to the menu.
pub(super) enum MenuOutcome {
    Open,
    Closed,
    Chosen(MessageAction),
}

impl MessageMenu {
    /// The menu of the message at `cell`: a prompt can be edited or rewound to, any other message
    /// forked from.
    pub(super) fn for_cell(cell: usize, is_prompt: bool) -> Self {
        let choices = if is_prompt {
            vec![MessageChoice::EditPrompt, MessageChoice::Rewind]
        } else {
            vec![MessageChoice::ForkThrough]
        };
        Self {
            cell,
            choices,
            selected: 0,
        }
    }

    pub(super) fn cell(&self) -> usize {
        self.cell
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> MenuOutcome {
        if key.kind == KeyEventKind::Release {
            return MenuOutcome::Open;
        }
        let chosen = |index: usize| {
            MenuOutcome::Chosen(MessageAction {
                cell: self.cell,
                choice: self.choices[index],
            })
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                MenuOutcome::Open
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.choices.len() - 1);
                MenuOutcome::Open
            }
            KeyCode::Enter => chosen(self.selected),
            KeyCode::Char(digit @ '1'..='9') => {
                let index = usize::from(digit as u8 - b'1');
                if index < self.choices.len() {
                    chosen(index)
                } else {
                    MenuOutcome::Open
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => MenuOutcome::Closed,
            _ => MenuOutcome::Open,
        }
    }

    /// Draw the menu as a box in `area`, one row per choice and then what the selected one does:
    /// below the message's rows (`message`, top and bottom), or above them when it does not fit
    /// there, so the message stays in view.
    pub(super) fn render(&self, area: Rect, message: Option<(u16, u16)>, buf: &mut Buffer) {
        let mut lines: Vec<Line<'static>> = self
            .choices
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                let number = format!("{}. ", index + 1);
                if index == self.selected {
                    Line::from(vec![
                        "› ".cyan(),
                        number.cyan(),
                        choice.label().cyan().bold(),
                    ])
                } else {
                    Line::from(vec!["  ".into(), number.into(), choice.label().into()])
                }
            })
            .collect();
        lines.push(Line::default());
        lines.push(Line::from(format!("  {}", self.choices[self.selected].detail())).dim());
        let widest = lines.iter().map(Line::width).max().unwrap_or(0);
        let width = u16::try_from(widest + 4)
            .unwrap_or(u16::MAX)
            .min(area.width.saturating_sub(4));
        let height = u16::try_from(lines.len() + 2)
            .unwrap_or(u16::MAX)
            .min(area.height);
        let y = match message {
            Some((_, bottom)) if bottom + height <= area.bottom() => bottom,
            Some((top, _)) if top >= area.y + height => top - height,
            _ => area.bottom().saturating_sub(height),
        };
        let menu = Rect::new(area.x + 2, y, width, height);
        Clear.render(menu, buf);
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(ratatui::style::Style::default().dim())
                    .title(" Message ")
                    .title_bottom(Line::from(" enter to choose · esc to close ").dim()),
            )
            .render(menu, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use pretty_assertions::assert_eq;

    fn press(menu: &mut MessageMenu, code: KeyCode) -> MenuOutcome {
        menu.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn chosen(outcome: MenuOutcome) -> Option<MessageAction> {
        match outcome {
            MenuOutcome::Chosen(action) => Some(action),
            MenuOutcome::Open | MenuOutcome::Closed => None,
        }
    }

    #[test]
    fn a_prompt_can_be_edited_or_rewound_to_and_any_other_message_forked_from() {
        assert_eq!(
            MessageMenu::for_cell(3, true).choices,
            vec![MessageChoice::EditPrompt, MessageChoice::Rewind]
        );
        assert_eq!(
            MessageMenu::for_cell(4, false).choices,
            vec![MessageChoice::ForkThrough]
        );
    }

    #[test]
    fn keys_move_the_selection_and_choose() {
        let mut menu = MessageMenu::for_cell(3, true);
        assert_eq!(
            chosen(press(&mut menu, KeyCode::Enter)),
            Some(MessageAction {
                cell: 3,
                choice: MessageChoice::EditPrompt
            })
        );
        press(&mut menu, KeyCode::Down);
        press(&mut menu, KeyCode::Down);
        assert_eq!(
            chosen(press(&mut menu, KeyCode::Enter)).map(|action| action.choice),
            Some(MessageChoice::Rewind)
        );
        assert_eq!(
            chosen(press(&mut menu, KeyCode::Char('1'))).map(|action| action.choice),
            Some(MessageChoice::EditPrompt)
        );
        assert!(chosen(press(&mut menu, KeyCode::Char('3'))).is_none());
        assert!(matches!(
            press(&mut menu, KeyCode::Esc),
            MenuOutcome::Closed
        ));
    }
}
