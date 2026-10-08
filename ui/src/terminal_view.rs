use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};

pub struct TerminalView<'a>(pub &'a vt100::Screen);

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
