mod terminal_view;

pub use terminal_view::TerminalView;

pub struct Ticket<'a> {
    pub identifier: &'a str,
    pub title: &'a str,
    pub status: &'a str,
    pub team: &'a str,
    pub url: &'a str,
    pub description: Option<&'a str>,
}

pub struct Tickets<'a> {
    pub items: &'a [Ticket<'a>],
    pub selected: usize,
    pub connected: bool,
    pub busy: bool,
}

impl Tickets<'_> {
    fn issue(&self) -> Option<&Ticket<'_>> {
        self.items.get(self.selected)
    }
}

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

pub struct Areas {
    pub sidebar: Rect,
    pub content: Rect,
    pub header: Rect,
}

pub fn areas(area: Rect) -> Areas {
    let sidebar_width = if area.width >= 70 { 28 } else { 0 };
    let height = area.height.saturating_sub(1);
    let content_y = area.y + area.height.min(1);
    Areas {
        sidebar: Rect::new(area.x, content_y, sidebar_width, height),
        content: Rect::new(
            area.x + sidebar_width,
            content_y,
            area.width.saturating_sub(sidebar_width),
            height,
        ),
        header: Rect::new(area.x, area.y, area.width, area.height.min(1)),
    }
}

pub fn draw(frame: &mut Frame, linear: &Tickets<'_>, focused: bool) -> Rect {
    let areas = areas(frame.area());
    frame.render_widget(
        Paragraph::new("Mentat").style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        areas.header,
    );
    if areas.sidebar.width > 0 {
        let block = Block::bordered()
            .title(" Workspace ")
            .border_style(if focused {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            });
        let inner = block.inner(areas.sidebar);
        frame.render_widget(block, areas.sidebar);
        let header_height = inner.height.min(3);
        let header = Rect::new(inner.x, inner.y, inner.width, header_height);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(if linear.busy {
                    "Linear: please wait"
                } else if linear.connected {
                    "Assigned issues"
                } else {
                    "Linear: not connected"
                }),
                Line::from(""),
            ]),
            header,
        );
        let list_area = Rect::new(
            inner.x,
            inner.y + header_height,
            inner.width,
            inner.height.saturating_sub(header_height),
        );
        let items: Vec<_> = linear
            .items
            .iter()
            .map(|issue| {
                ListItem::new(vec![
                    Line::from(issue.identifier),
                    Line::from(format!("  {}", issue.title)),
                ])
            })
            .collect();
        let mut state = ListState::default().with_selected(linear.issue().map(|_| linear.selected));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
                .highlight_symbol("› "),
            list_area,
            &mut state,
        );
    }
    areas.content
}

pub fn details(linear: &Tickets<'_>, notice: &str) -> String {
    let mut text = if let Some(issue) = linear.issue() {
        format!(
            "{} — {}\n\nStatus: {}\nTeam: {}\n{}\n\n{}",
            issue.identifier,
            issue.title,
            issue.status,
            issue.team,
            issue.url,
            issue
                .description
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("No description.")
        )
    } else if linear.connected {
        "No assigned Linear tickets.\n\nSelect a ticket when the list contains issues.".into()
    } else {
        "Connect Linear to get started".into()
    };
    if !notice.is_empty() {
        text.push_str(&format!("\n\n{notice}"));
    }
    text
}

pub fn draw_details(frame: &mut Frame, area: Rect, text: String) {
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Ticket overview ")),
        area,
    );
}

pub fn help(frame: &mut Frame, escape: char, editor: &str, pane_navigation: bool) {
    let area = frame.area();
    let width = area.width.min(62);
    let height = area.height.min(23);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let navigation = if pane_navigation {
        "  Ctrl-h / l   Focus sidebar / last terminal\n  Ctrl-j / k   Down / up (no vertical panes yet)\n"
    } else {
        ""
    };
    let text = format!(
        "\n  Sidebar focus\n  j / k        Select issue\n  c            Open ticket agent and paste title/body\n  Enter        Open ticket terminal without paste\n  e            Open {editor}\n  t            Open a plain shell\n  l            Sign in to Linear\n  r            Refresh Linear issues\n  s            Show status\n  q / Ctrl-C   Quit (ask before stopping editor or shell)\n\n  Pane navigation\n{navigation}  Terminal focus\n  Ctrl-{escape}       Return to the ticket overview\n  Other keys go to the active terminal.\n\n  Esc or ? closes this popup."
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .title(" Help ")
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        popup,
    );
}

pub fn status_popup(frame: &mut Frame, text: String) {
    let area = frame.area();
    let width = area.width.min(76);
    let height = area.height.min(18);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!("{text}\n\nEsc or s closes this popup."))
            .wrap(Wrap { trim: false })
            .block(
                Block::bordered()
                    .title(" Status ")
                    .border_style(Style::default().fg(Color::Cyan)),
            ),
        popup,
    );
}

/// Draw a borrowed terminal screen without owning its session or lock.
pub fn draw_terminal(frame: &mut Frame, area: Rect, screen: &vt100::Screen, show_cursor: bool) {
    frame.render_widget(TerminalView(screen), area);
    let (row, col) = screen.cursor_position();
    if show_cursor && !screen.hide_cursor() && row < area.height && col < area.width {
        frame.set_cursor_position((area.x + col, area.y + row));
    }
}

pub fn quit_popup(frame: &mut Frame) {
    let area = frame.area();
    let width = area.width.min(64);
    let height = area.height.min(9);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new("An editor or shell is still open.\n\nForce quit loses unsaved buffers and stops shell jobs.\nWorkspace files and Agent session IDs remain saved.\n\nf: Force quit    Esc: Return and close tools yourself")
        .wrap(Wrap { trim: false }).block(Block::bordered().title(" Quit Mentat ")), popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    #[test]
    fn draws_without_services_at_narrow_and_wide_sizes() {
        let tickets = Tickets {
            items: &[],
            selected: 0,
            connected: false,
            busy: false,
        };
        assert_eq!(details(&tickets, ""), "Connect Linear to get started");
        for width in [0, 40, 100] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| {
                    let content = draw(frame, &tickets, true);
                    assert_eq!(content.width, if width >= 70 { width - 28 } else { width });
                    draw_details(frame, content, details(&tickets, ""));
                })
                .unwrap();
        }
    }
}
