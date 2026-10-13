use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
pub use vt100::{MouseProtocolEncoding, MouseProtocolMode};

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
        KeyCode::Enter if shift || ctrl => return format!("\x1b[13;{modifier}u").into_bytes(),
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

/// Encode a mouse event whose zero-based coordinates are relative to the child.
pub fn mouse(
    event: MouseEvent,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Vec<u8> {
    use MouseEventKind::*;
    use MouseProtocolMode as Mode;
    if mode == Mode::None
        || (mode == Mode::Press && !matches!(event.kind, Down(_)))
        || (matches!(event.kind, Drag(_)) && !matches!(mode, Mode::ButtonMotion | Mode::AnyMotion))
        || (event.kind == Moved && mode != Mode::AnyMotion)
    {
        return Vec::new();
    }
    let button = |button| match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let release = matches!(event.kind, Up(_));
    let mut code = match event.kind {
        Down(b) | Up(b) => button(b),
        Drag(b) => button(b) + 32,
        Moved => 35,
        ScrollUp => 64,
        ScrollDown => 65,
        ScrollLeft => 66,
        ScrollRight => 67,
    };
    if release && encoding != MouseProtocolEncoding::Sgr {
        code = 3;
    }
    if mode != Mode::Press {
        code += 4 * u8::from(event.modifiers.contains(KeyModifiers::SHIFT))
            + 8 * u8::from(event.modifiers.contains(KeyModifiers::ALT))
            + 16 * u8::from(event.modifiers.contains(KeyModifiers::CONTROL));
    }
    let x = u32::from(event.column) + 1;
    let y = u32::from(event.row) + 1;
    match encoding {
        MouseProtocolEncoding::Sgr => {
            let suffix = if release { 'm' } else { 'M' };
            format!("\x1b[<{code};{x};{y}{suffix}").into_bytes()
        }
        MouseProtocolEncoding::Default if x <= 223 && y <= 223 => {
            vec![0x1b, b'[', b'M', code + 32, x as u8 + 32, y as u8 + 32]
        }
        MouseProtocolEncoding::Utf8 if x <= 2015 && y <= 2015 => format!(
            "\x1b[M{}{}{}",
            char::from(code + 32),
            char::from_u32(x + 32).unwrap(),
            char::from_u32(y + 32).unwrap(),
        )
        .into_bytes(),
        _ => Vec::new(),
    }
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
    fn mouse_respects_tracking_modes_and_encodings() {
        use MouseProtocolEncoding as Encoding;
        use MouseProtocolMode as Mode;
        let mut event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert!(mouse(event, Mode::None, Encoding::Sgr).is_empty());
        assert_eq!(
            mouse(event, Mode::PressRelease, Encoding::Sgr),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            mouse(event, Mode::PressRelease, Encoding::Default),
            b"\x1b[M %#"
        );
        event.kind = MouseEventKind::Up(MouseButton::Left);
        assert!(mouse(event, Mode::Press, Encoding::Sgr).is_empty());
        assert_eq!(
            mouse(event, Mode::PressRelease, Encoding::Sgr),
            b"\x1b[<0;5;3m"
        );
        assert_eq!(
            mouse(event, Mode::PressRelease, Encoding::Default),
            b"\x1b[M#%#"
        );
        event.kind = MouseEventKind::Drag(MouseButton::Right);
        event.modifiers = KeyModifiers::CONTROL;
        assert!(mouse(event, Mode::PressRelease, Encoding::Sgr).is_empty());
        assert_eq!(
            mouse(event, Mode::ButtonMotion, Encoding::Sgr),
            b"\x1b[<50;5;3M"
        );
        event.kind = MouseEventKind::Moved;
        assert!(mouse(event, Mode::ButtonMotion, Encoding::Sgr).is_empty());
        assert_eq!(
            mouse(event, Mode::AnyMotion, Encoding::Sgr),
            b"\x1b[<51;5;3M"
        );
        event.kind = MouseEventKind::ScrollDown;
        event.modifiers = KeyModifiers::NONE;
        assert_eq!(
            mouse(event, Mode::PressRelease, Encoding::Sgr),
            b"\x1b[<65;5;3M"
        );
        event.column = 223;
        assert!(mouse(event, Mode::PressRelease, Encoding::Default).is_empty());
        assert_eq!(
            mouse(event, Mode::PressRelease, Encoding::Utf8),
            "\x1b[Ma\u{100}#".as_bytes()
        );
    }

    #[test]
    fn enter_preserves_modifiers_and_backspace_remains_delete() {
        for (modifiers, expected) in [
            (KeyModifiers::NONE, b"\r".as_slice()),
            (KeyModifiers::SHIFT, b"\x1b[13;2u".as_slice()),
            (KeyModifiers::CONTROL, b"\x1b[13;5u".as_slice()),
            (KeyModifiers::ALT, b"\x1b\r".as_slice()),
            (
                KeyModifiers::SHIFT | KeyModifiers::ALT,
                b"\x1b[13;4u".as_slice(),
            ),
        ] {
            assert_eq!(
                encode(KeyEvent::new(KeyCode::Enter, modifiers), false),
                expected
            );
        }
        assert_eq!(
            encode(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), false),
            b"\x7f"
        );
    }

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
