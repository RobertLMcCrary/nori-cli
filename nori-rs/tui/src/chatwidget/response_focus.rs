//! Focused-message header and the actions represented by its icons.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::style::Stylize;
use ratatui::text::Line;

#[derive(Default)]
pub(super) struct ResponseFocus {
    active: bool,
    sent_prompt: bool,
}

const RESPONSE_ACTIONS: [(char, KeyCode, KeyModifiers); 5] = [
    ('‹', KeyCode::Char('h'), KeyModifiers::CONTROL),
    ('›', KeyCode::Char('l'), KeyModifiers::CONTROL),
    ('↶', KeyCode::Enter, KeyModifiers::NONE),
    ('⑂', KeyCode::Char(' '), KeyModifiers::NONE),
    ('×', KeyCode::Esc, KeyModifiers::NONE),
];
const PROMPT_ACTIONS: [(char, KeyCode, KeyModifiers); 2] = [
    ('‹', KeyCode::Char('h'), KeyModifiers::CONTROL),
    ('×', KeyCode::Esc, KeyModifiers::NONE),
];

fn actions(sent_prompt: bool) -> &'static [(char, KeyCode, KeyModifiers)] {
    if sent_prompt {
        &PROMPT_ACTIONS
    } else {
        &RESPONSE_ACTIONS
    }
}

pub(crate) fn header(width: u16, sent_prompt: bool) -> Line<'static> {
    let mut text = String::from("╭─ ");
    for (icon, _, _) in actions(sent_prompt) {
        text.push(*icon);
        text.push(' ');
    }
    let used = text.chars().count();
    text.extend(std::iter::repeat_n(
        '─',
        usize::from(width).saturating_sub(used),
    ));
    text.chars()
        .take(usize::from(width))
        .collect::<String>()
        .cyan()
        .into()
}

/// Each visible icon and its trailing space form a non-overlapping touch target.
pub(super) fn action_at(column: u16, width: u16, sent_prompt: bool) -> Option<KeyEvent> {
    actions(sent_prompt)
        .iter()
        .enumerate()
        .find_map(|(index, (_, code, modifiers))| {
            let x = 3 + u16::try_from(index).ok()? * 2;
            (x < width && column >= x && column < x.saturating_add(2).min(width))
                .then_some(KeyEvent::new(*code, *modifiers))
        })
}

impl ResponseFocus {
    pub(super) fn set_active(&mut self, active: bool) {
        self.active = active;
        self.sent_prompt = false;
    }

    pub(super) fn set_sent_prompt(&mut self, sent_prompt: bool) {
        self.sent_prompt = sent_prompt;
    }

    pub(super) fn active(&self) -> bool {
        self.active
    }

    pub(super) fn sent_prompt(&self) -> bool {
        self.sent_prompt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn icons_only_activate_their_own_visible_cells() {
        assert_eq!(
            action_at(3, 30, false),
            Some(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            action_at(7, 30, false),
            Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        );
        assert_eq!(action_at(9, 9, false), None);
        assert_eq!(
            action_at(4, 30, false),
            Some(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL))
        );
        assert_eq!(action_at(2, 30, false), None);
        assert_eq!(
            action_at(5, 30, true),
            Some(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        );
        assert_eq!(action_at(7, 30, true), None);
    }
}
