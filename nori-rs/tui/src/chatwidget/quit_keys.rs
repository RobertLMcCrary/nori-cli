//! Application quit chords, independent of the selected conversation or editor mode.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuitKeyAction {
    Continue,
    Interrupt,
    Exit,
}

#[derive(Default)]
pub(super) struct QuitKeys {
    armed: bool,
    pub(super) immediate: bool,
}

impl QuitKeys {
    pub(super) fn handle(&mut self, key: KeyEvent) -> QuitKeyAction {
        if key.kind != KeyEventKind::Press {
            return QuitKeyAction::Continue;
        }
        if key.modifiers == KeyModifiers::CONTROL {
            if key.code == KeyCode::Char('d') {
                return QuitKeyAction::Exit;
            }
            if key.code == KeyCode::Char('c') {
                if self.armed {
                    return QuitKeyAction::Exit;
                }
                self.armed = true;
                return QuitKeyAction::Interrupt;
            }
        }
        self.armed = false;
        QuitKeyAction::Continue
    }
}

impl super::ChatWidget {
    pub(crate) fn quit_key_action(&mut self, key: KeyEvent) -> QuitKeyAction {
        self.quit_keys.handle(key)
    }

    pub(crate) fn begin_immediate_exit(&mut self) {
        self.quit_keys.immediate = true;
        if self.external_cli_login_active() {
            self.route_external_cli_login_key_event(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            ));
        }
        self.begin_exit();
    }

    pub(crate) fn immediate_exit_requested(&self) -> bool {
        self.quit_keys.immediate
    }

    pub(crate) fn interrupt_for_quit_key(&mut self) {
        if self.bottom_pane.on_ctrl_c() == super::CancellationEvent::Handled {
            return;
        }
        if self.bottom_pane.is_task_running() {
            self.submit_harness_action(crate::app_event::HarnessAction::Cancel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn quit_requires_two_presses_and_never_auto_repeat() {
        let mut keys = QuitKeys::default();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(keys.handle(key), QuitKeyAction::Interrupt);
        assert_eq!(
            keys.handle(KeyEvent {
                kind: KeyEventKind::Repeat,
                ..key
            }),
            QuitKeyAction::Continue
        );
        assert_eq!(keys.handle(key), QuitKeyAction::Exit);
    }

    #[test]
    fn other_input_disarms_quit_and_ctrl_d_always_exits() {
        let mut keys = QuitKeys::default();
        let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(keys.handle(interrupt), QuitKeyAction::Interrupt);
        assert_eq!(
            keys.handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            QuitKeyAction::Continue
        );
        assert_eq!(keys.handle(interrupt), QuitKeyAction::Interrupt);
        assert_eq!(
            keys.handle(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            QuitKeyAction::Exit
        );
    }
}
