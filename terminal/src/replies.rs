use crossterm::cursor::SetCursorStyle;

/// Reply locally, including while hidden. Never pass queries to the host terminal.
pub struct Replies {
    pub output: Vec<u8>,
    pub cursor_style: SetCursorStyle,
}

impl Default for Replies {
    fn default() -> Self {
        Self {
            output: Vec::new(),
            cursor_style: SetCursorStyle::DefaultUserShape,
        }
    }
}

impl vt100::Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let param = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        if (i1, i2, c) == (Some(b' '), None, 'q') {
            self.cursor_style = match param {
                0 => SetCursorStyle::DefaultUserShape,
                1 => SetCursorStyle::BlinkingBlock,
                2 => SetCursorStyle::SteadyBlock,
                3 => SetCursorStyle::BlinkingUnderScore,
                4 => SetCursorStyle::SteadyUnderScore,
                5 => SetCursorStyle::BlinkingBar,
                6 => SetCursorStyle::SteadyBar,
                _ => return,
            };
            return;
        }
        let reply = match (i1, i2, c, param) {
            (None, None, 'n', 5) => "\x1b[0n".to_owned(),
            (None, None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                format!("\x1b[{};{}R", row + 1, col + 1)
            }
            (None, None, 'c', 0) => "\x1b[?1;2c".to_owned(),
            (Some(b'>'), None, 'c', 0) => "\x1b[>0;0;0c".to_owned(),
            _ => return,
        };
        self.output.extend_from_slice(reply.as_bytes());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        match params {
            [b"10", b"?"] => self
                .output
                .extend_from_slice(b"\x1b]10;rgb:dddd/dddd/dddd\x1b\\"),
            [b"11", b"?"] => self
                .output
                .extend_from_slice(b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn answers_queries_without_a_view() {
        let mut parser = vt100::Parser::new_with_callbacks(4, 20, 0, Replies::default());
        parser.process(b"\x1b[2;6H\x1b[6n\x1b[?2004h");
        assert_eq!(parser.callbacks().output, b"\x1b[2;6R");
        assert!(parser.screen().bracketed_paste());
    }

    #[test]
    fn retains_cursor_style_across_split_sequences_and_ignores_invalid_styles() {
        let mut parser = vt100::Parser::new_with_callbacks(4, 20, 0, Replies::default());
        for style in 0..=6 {
            parser.process(format!("\x1b[{style}").as_bytes());
            parser.process(b" q");
            assert_eq!(
                parser.callbacks().cursor_style.to_string(),
                format!("\x1b[{style} q")
            );
        }
        parser.process(b"\x1b[99 q");
        assert_eq!(parser.callbacks().cursor_style, SetCursorStyle::SteadyBar);
        assert!(parser.callbacks().output.is_empty());
    }
}
