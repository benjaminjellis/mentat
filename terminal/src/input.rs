use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub fn encode(key: KeyEvent, application_cursor: bool) -> Vec<u8> {
    if key.kind == KeyEventKind::Release {
        return Vec::new();
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let mut bytes = Vec::new();
    let sequence = match key.code {
        KeyCode::Char(c) => {
            if alt {
                bytes.push(0x1b);
            }
            if ctrl {
                match c.to_ascii_lowercase() {
                    'a'..='z' => bytes.push(c.to_ascii_lowercase() as u8 - b'a' + 1),
                    ' ' | '@' => bytes.push(0),
                    // Crossterm uses these aliases for legacy control bytes 0x1c..0x1f.
                    '4'..='7' => bytes.push(c as u8 - b'4' + 0x1c),
                    '['..='_' => bytes.push(c as u8 & 0x1f),
                    '?' => bytes.push(0x7f),
                    _ => {}
                }
            } else {
                bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
            return bytes;
        }
        KeyCode::Enter => "\r".to_owned(),
        KeyCode::Tab => "\t".to_owned(),
        KeyCode::BackTab => "\x1b[Z".to_owned(),
        KeyCode::Backspace => "\x7f".to_owned(),
        KeyCode::Esc => "\x1b".to_owned(),
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Right
        | KeyCode::Left
        | KeyCode::Home
        | KeyCode::End => {
            let suffix = match key.code {
                KeyCode::Up => 'A',
                KeyCode::Down => 'B',
                KeyCode::Right => 'C',
                KeyCode::Left => 'D',
                KeyCode::Home => 'H',
                _ => 'F',
            };
            return if modifier > 1 {
                format!("\x1b[1;{modifier}{suffix}")
            } else {
                format!("\x1b{}{suffix}", if application_cursor { 'O' } else { '[' })
            }
            .into_bytes();
        }
        KeyCode::Insert
        | KeyCode::Delete
        | KeyCode::PageUp
        | KeyCode::PageDown
        | KeyCode::F(5..=12) => {
            let number = match key.code {
                KeyCode::Insert => 2,
                KeyCode::Delete => 3,
                KeyCode::PageUp => 5,
                KeyCode::PageDown => 6,
                KeyCode::F(n) => [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)],
                _ => unreachable!(),
            };
            return if modifier > 1 {
                format!("\x1b[{number};{modifier}~")
            } else {
                format!("\x1b[{number}~")
            }
            .into_bytes();
        }
        KeyCode::F(n @ 1..=4) => {
            let suffix = char::from(b'P' + n - 1);
            return if modifier > 1 {
                format!("\x1b[1;{modifier}{suffix}")
            } else {
                format!("\x1bO{suffix}")
            }
            .into_bytes();
        }
        _ => return bytes,
    };
    if alt {
        bytes.push(0x1b);
    }
    bytes.extend_from_slice(sequence.as_bytes());
    bytes
}

pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_follow_child_modes() {
        assert_eq!(
            encode(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true),
            b"\x1bOA"
        );
        assert_eq!(
            encode(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL), false),
            b"\x1b[1;5D"
        );
        assert_eq!(
            encode(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false
            ),
            [3]
        );
        assert_eq!(paste("one\ntwo", true), b"\x1b[200~one\ntwo\x1b[201~");
    }
}
