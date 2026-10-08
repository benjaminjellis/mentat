/// Reply locally, including while hidden. Never pass queries to the host terminal.
#[derive(Default)]
pub struct Replies(pub Vec<u8>);

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
        self.0.extend_from_slice(reply.as_bytes());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        match params {
            [b"10", b"?"] => self
                .0
                .extend_from_slice(b"\x1b]10;rgb:dddd/dddd/dddd\x1b\\"),
            [b"11", b"?"] => self
                .0
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
        assert_eq!(parser.callbacks().0, b"\x1b[2;6R");
        assert!(parser.screen().bracketed_paste());
    }
}
