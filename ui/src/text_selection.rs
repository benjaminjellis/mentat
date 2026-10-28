use ratatui::{buffer::Buffer, layout::Rect, style::Color, text::Line, widgets::Widget};

/// Copy the visible overview text without its border or adjacent panes.
pub struct TextSelection {
    buffer: Buffer,
    start: (u16, u16),
    end: (u16, u16),
    pub dragging: bool,
}

impl TextSelection {
    pub fn new(buffer: Buffer, row: u16, col: u16) -> Self {
        Self {
            buffer,
            start: (row, col),
            end: (row, col),
            dragging: true,
        }
    }

    pub fn update(&mut self, row: u16, col: u16) {
        self.end = (
            row.min(self.buffer.area.height.saturating_sub(1)),
            col.min(self.buffer.area.width.saturating_sub(1)),
        );
    }

    pub fn moved(&self) -> bool {
        self.start != self.end
    }

    fn bounds(&self) -> ((u16, u16), (u16, u16)) {
        let (mut start, mut end) = (self.start.min(self.end), self.start.max(self.end));
        for point in [&mut start, &mut end] {
            for col in 0..point.1 {
                let width = Line::raw(self.buffer[(col, point.0)].symbol()).width() as u16;
                if col + width > point.1 {
                    point.1 = col;
                    break;
                }
            }
        }
        end.1 += Line::raw(self.buffer[(end.1, end.0)].symbol())
            .width()
            .max(1) as u16;
        (start, end)
    }

    pub fn text(&self) -> String {
        let (start, end) = self.bounds();
        let mut lines = Vec::new();
        for row in start.0..=end.0 {
            let mut line = String::new();
            let mut col = if row == start.0 { start.1 } else { 0 };
            let limit = if row == end.0 {
                end.1
            } else {
                self.buffer.area.width
            };
            while col < limit {
                let symbol = self.buffer[(col, row)].symbol();
                line.push_str(symbol);
                col += Line::raw(symbol).width().max(1) as u16;
            }
            lines.push(line.trim_end().to_owned());
        }
        lines.join("\n")
    }
}

impl Widget for &TextSelection {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let (start, end) = self.bounds();
        for row in 0..area.height.min(self.buffer.area.height) {
            for col in 0..area.width.min(self.buffer.area.width) {
                let cell = &mut buffer[(area.x + col, area.y + row)];
                *cell = self.buffer[(col, row)].clone();
                if (row, col) >= start && (row, col) < end {
                    cell.set_fg(Color::Black).set_bg(Color::Rgb(193, 163, 113));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Text;

    #[test]
    fn overview_selection_copies_scrolled_unicode_text_in_both_directions() {
        let area = Rect::new(30, 2, 12, 3);
        let text = Text::raw("hidden\na界é text\nsecond line");
        let mut selection = crate::select_details(text.clone(), area, 1, 0, 2);
        selection.update(1, 5);
        assert_eq!(selection.text(), "界é text\nsecond");
        let mut reverse = crate::select_details(text, area, 1, 1, 5);
        reverse.update(0, 2);
        assert_eq!(reverse.text(), selection.text());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 50, 8));
        (&selection).render(area, &mut buffer);
        assert_eq!(buffer[(31, 2)].bg, Color::Rgb(193, 163, 113));
        assert_eq!(buffer[(29, 2)].bg, Color::Reset);
        selection.update(u16::MAX, u16::MAX);
        assert!(selection.text().contains("second line"));
    }
}
