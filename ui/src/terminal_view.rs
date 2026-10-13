use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};

pub struct TerminalView<'a>(pub &'a vt100::Screen);

/// A stable snapshot lets a drag select output even while the PTY is updating.
pub struct TerminalSelection {
    pub screen: vt100::Screen,
    start: (u16, u16),
    end: (u16, u16),
    pub dragging: bool,
}

impl TerminalSelection {
    pub fn new(screen: vt100::Screen, row: u16, col: u16) -> Self {
        Self {
            screen,
            start: (row, col),
            end: (row, col),
            dragging: true,
        }
    }

    pub fn update(&mut self, row: u16, col: u16) {
        let (rows, cols) = self.screen.size();
        self.end = (
            row.min(rows.saturating_sub(1)),
            col.min(cols.saturating_sub(1)),
        );
    }

    fn bounds(&self) -> ((u16, u16), (u16, u16)) {
        let (mut start, mut end) = (self.start.min(self.end), self.start.max(self.end));
        // Include the complete glyph when a drag ends on either half of a wide cell.
        if self
            .screen
            .cell(start.0, start.1)
            .is_some_and(|c| c.is_wide_continuation())
        {
            start.1 = start.1.saturating_sub(1);
        }
        end.1 += if self.screen.cell(end.0, end.1).is_some_and(|c| c.is_wide()) {
            2
        } else {
            1
        };
        (start, end)
    }

    pub fn text(&self) -> String {
        let (start, end) = self.bounds();
        self.screen.contents_between(start.0, start.1, end.0, end.1)
    }

    pub fn moved(&self) -> bool {
        self.start != self.end
    }
}

impl Widget for &TerminalSelection {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        TerminalView(&self.screen).render(area, buffer);
        let (start, end) = self.bounds();
        for row in 0..area.height {
            for col in 0..area.width {
                if (row, col) >= start && (row, col) < end {
                    buffer[(area.x + col, area.y + row)]
                        .set_fg(Color::Black)
                        .set_bg(Color::Rgb(193, 163, 113));
                }
            }
        }
    }
}

impl Widget for TerminalView<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        for row in 0..area.height {
            for col in 0..area.width {
                let Some(cell) = self.0.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let mut style = Style::default()
                    .fg(color(cell.fgcolor()))
                    .bg(color(cell.bgcolor()));
                for (enabled, modifier) in [
                    (cell.bold(), Modifier::BOLD),
                    (cell.dim(), Modifier::DIM),
                    (cell.italic(), Modifier::ITALIC),
                    (cell.underline(), Modifier::UNDERLINED),
                    (cell.inverse(), Modifier::REVERSED),
                ] {
                    if enabled {
                        style = style.add_modifier(modifier);
                    }
                }
                let contents = cell.contents();
                buffer[(area.x + col, area.y + row)]
                    .set_symbol(if contents.is_empty() { " " } else { contents })
                    .set_style(style);
            }
        }
    }
}

fn color(value: vt100::Color) -> Color {
    match value {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_copies_only_pane_text_in_either_direction() {
        let mut parser = vt100::Parser::new(4, 12, 0);
        parser.process(b"first line\r\nsecond line");
        let mut selection = TerminalSelection::new(parser.screen().clone(), 0, 6);
        selection.update(1, 5);
        assert_eq!(selection.text(), "line\nsecond");
        let mut reverse = TerminalSelection::new(parser.screen().clone(), 1, 5);
        reverse.update(0, 6);
        assert_eq!(reverse.text(), selection.text());
        // Live output cannot change the snapshot being copied.
        parser.process(b"\r\x1b[2Kreplacement");
        assert_eq!(selection.text(), "line\nsecond");
        let area = Rect::new(28, 1, 12, 4);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 5));
        (&selection).render(area, &mut buffer);
        assert_eq!(buffer[(34, 1)].bg, Color::Rgb(193, 163, 113));
        assert_eq!(buffer[(28, 2)].bg, Color::Rgb(193, 163, 113));
        assert_eq!(buffer[(27, 2)].bg, Color::Reset);
        assert_eq!(buffer[(33, 1)].bg, Color::Reset);
    }

    #[test]
    fn selection_handles_wide_glyphs_and_soft_wrapped_lines() {
        let mut parser = vt100::Parser::new(3, 6, 0);
        parser.process("a界é12345".as_bytes());
        let mut selection = TerminalSelection::new(parser.screen().clone(), 0, 2);
        selection.update(1, 2);
        assert_eq!(selection.text(), "界é12345");
        let mut selection = TerminalSelection::new(parser.screen().clone(), 0, 0);
        selection.update(0, 1);
        assert_eq!(selection.text(), "a界");
    }

    #[test]
    fn renders_alternate_screen_unicode_and_colors() {
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process("primary\x1b[?1049h\x1b[2;3H\x1b[38;2;1;2;3m界é".as_bytes());
        let area = Rect::new(0, 0, 20, 4);
        let mut buffer = Buffer::empty(area);
        TerminalView(parser.screen()).render(area, &mut buffer);
        assert_eq!(buffer[(2, 1)].symbol(), "界");
        assert_eq!(buffer[(2, 1)].fg, Color::Rgb(1, 2, 3));
        assert_eq!(buffer[(4, 1)].symbol(), "é");
        parser.process(b"\x1b[?1049l");
        assert_eq!(parser.screen().contents(), "primary");
    }
}
