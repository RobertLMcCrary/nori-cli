/// Key input types
pub enum Key {
    Enter,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Backspace,
    Tab,
    ShiftEnter,
    AltEnter,
    Ctrl(char),
    Char(char),
}

impl Key {
    pub fn to_escape_sequence(&self) -> Vec<u8> {
        match self {
            Key::Enter => vec![b'\r'],
            Key::Escape => vec![0x1b],
            Key::Up => vec![0x1b, b'[', b'A'],
            Key::Down => vec![0x1b, b'[', b'B'],
            Key::Right => vec![0x1b, b'[', b'C'],
            Key::Left => vec![0x1b, b'[', b'D'],
            Key::Backspace => vec![0x7f],
            Key::Tab => vec![b'\t'],
            Key::ShiftEnter => b"\x1b[13;2u".to_vec(),
            Key::AltEnter => b"\x1b[13;3u".to_vec(),
            Key::Ctrl(c) => vec![(*c as u8) & 0x1f],
            Key::Char(c) => c.to_string().into_bytes(),
        }
    }
}
