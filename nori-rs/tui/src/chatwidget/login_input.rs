use super::*;

impl ChatWidget {
    pub(crate) fn external_cli_login_active(&self) -> bool {
        self.login_handler
            .as_ref()
            .is_some_and(LoginHandler::has_external_cli_task)
    }

    /// Give the external login terminal exclusive access to keyboard input.
    pub(crate) fn route_external_cli_login_key_event(&mut self, key: KeyEvent) -> bool {
        let Some(handler) = self.login_handler.as_ref() else {
            return false;
        };
        if !handler.has_external_cli_task() {
            return false;
        }
        if key.kind == KeyEventKind::Release {
            return true;
        }
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            if let Some(mut handler) = self.login_handler.take() {
                handler.cancel();
            }
            self.app_event_tx.send(AppEvent::LoginCancelled);
            self.login_agent_override = None;
            self.add_login_message("Login cancelled.".to_string(), None);
            self.login_child_target = None;
            self.request_redraw();
            return true;
        }

        let bytes = match key.code {
            KeyCode::Char(ch)
                if key.modifiers.contains(KeyModifiers::CONTROL) && ch.is_ascii_alphabetic() =>
            {
                vec![(ch.to_ascii_lowercase() as u8) - b'a' + 1]
            }
            KeyCode::Char(ch) => ch.to_string().into_bytes(),
            KeyCode::Enter => vec![b'\r'],
            KeyCode::Backspace => vec![0x7f],
            KeyCode::Tab => vec![b'\t'],
            KeyCode::BackTab => b"\x1b[Z".to_vec(),
            KeyCode::Up => b"\x1b[A".to_vec(),
            KeyCode::Down => b"\x1b[B".to_vec(),
            KeyCode::Right => b"\x1b[C".to_vec(),
            KeyCode::Left => b"\x1b[D".to_vec(),
            KeyCode::Home => b"\x1b[H".to_vec(),
            KeyCode::End => b"\x1b[F".to_vec(),
            KeyCode::Delete => b"\x1b[3~".to_vec(),
            KeyCode::Insert => b"\x1b[2~".to_vec(),
            KeyCode::PageUp => b"\x1b[5~".to_vec(),
            KeyCode::PageDown => b"\x1b[6~".to_vec(),
            _ => Vec::new(),
        };
        if !bytes.is_empty() {
            handler.send_pty_input(bytes);
        }
        true
    }

    /// Forward pasted login input without placing it in the chat composer.
    pub(crate) fn route_external_cli_login_paste(&mut self, text: String) -> bool {
        let Some(handler) = self.login_handler.as_ref() else {
            return false;
        };
        if !handler.has_external_cli_task() {
            return false;
        }
        handler.send_pty_input(text.into_bytes());
        true
    }
}
