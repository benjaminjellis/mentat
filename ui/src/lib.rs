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
    text::{Line, Span, Text},
    widgets::{Block, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};

const TICKET_ACCENT: Color = Color::Rgb(193, 163, 113);
const TICKET_TEXT: Color = Color::Rgb(224, 215, 198);
const TICKET_MUTED: Color = Color::Rgb(145, 143, 133);
const TICKET_STATUS: Color = Color::Rgb(161, 182, 151);
const TICKET_SELECTED: Color = Color::Rgb(57, 53, 45);

pub struct Areas {
    pub sidebar: Rect,
    pub content: Rect,
    pub header: Rect,
}

pub fn areas(area: Rect, sidebar_visible: bool) -> Areas {
    if !sidebar_visible {
        return Areas {
            sidebar: Rect::default(),
            content: area,
            header: Rect::default(),
        };
    }
    let sidebar_width = if sidebar_visible && area.width >= 70 {
        28
    } else {
        0
    };
    let header_height = area.height.min(1);
    let height = area.height.saturating_sub(header_height);
    let content_y = area.y + header_height;
    Areas {
        sidebar: Rect::new(area.x, content_y, sidebar_width, height),
        content: Rect::new(
            area.x + sidebar_width,
            content_y,
            area.width.saturating_sub(sidebar_width),
            height,
        ),
        header: Rect::new(area.x, area.y, area.width, header_height),
    }
}

pub fn draw(frame: &mut Frame, linear: &Tickets<'_>, focused: bool, sidebar_visible: bool) -> Rect {
    let areas = areas(frame.area(), sidebar_visible);
    if !sidebar_visible {
        return areas.content;
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "MENTAT",
                Style::default()
                    .fg(Color::Rgb(218, 205, 180))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled("≋", Style::default().fg(Color::Rgb(155, 133, 98))),
            Span::styled("≋", Style::default().fg(Color::Rgb(193, 163, 113))),
            Span::styled("≋", Style::default().fg(Color::Rgb(231, 193, 128))),
        ])),
        areas.header,
    );
    if areas.sidebar.width > 0 {
        let refresh_title = if linear.busy {
            const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let tick = START
                .get_or_init(std::time::Instant::now)
                .elapsed()
                .as_millis()
                / 100;
            format!(" {} ", SPINNER[tick as usize % SPINNER.len()])
        } else {
            String::new()
        };
        let block = Block::bordered()
            .title(" Workspace ")
            .title(Line::from(refresh_title).right_aligned())
            .border_style(if focused {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            });
        let inner = block.inner(areas.sidebar);
        frame.render_widget(block, areas.sidebar);
        let items: Vec<_> = linear
            .items
            .iter()
            .map(|issue| {
                ListItem::new(vec![
                    Line::from(Span::styled(
                        issue.identifier,
                        Style::default()
                            .fg(TICKET_ACCENT)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(
                        format!("  {}", issue.title),
                        Style::default().fg(TICKET_TEXT),
                    )),
                ])
            })
            .collect();
        let mut state = ListState::default().with_selected(linear.issue().map(|_| linear.selected));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(
                    Style::default()
                        .bg(TICKET_SELECTED)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("› "),
            inner,
            &mut state,
        );
    }
    areas.content
}

pub fn details(linear: &Tickets<'_>, notice: &str) -> Text<'static> {
    let muted = Style::default().fg(TICKET_MUTED);
    let mut text = if let Some(issue) = linear.issue() {
        let mut lines = vec![
            Line::from(Span::styled(
                issue.identifier.to_owned(),
                Style::default()
                    .fg(TICKET_ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                issue.title.to_owned(),
                Style::default()
                    .fg(TICKET_TEXT)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::default(),
            Line::from(vec![
                Span::styled("Status  ", muted),
                Span::styled(issue.status.to_owned(), Style::default().fg(TICKET_STATUS)),
            ]),
            Line::from(vec![
                Span::styled("Team    ", muted),
                Span::styled(issue.team.to_owned(), Style::default().fg(TICKET_TEXT)),
            ]),
            Line::from(Span::styled(
                issue.url.to_owned(),
                Style::default()
                    .fg(Color::Rgb(151, 169, 177))
                    .add_modifier(Modifier::UNDERLINED),
            )),
            Line::default(),
        ];
        let description = issue.description.filter(|s| !s.trim().is_empty());
        lines.extend(
            description
                .unwrap_or("No description.")
                .lines()
                .map(|line| {
                    Line::from(Span::styled(
                        line.to_owned(),
                        if description.is_some() {
                            Style::default().fg(TICKET_TEXT)
                        } else {
                            muted
                        },
                    ))
                }),
        );
        Text::from(lines)
    } else if linear.busy {
        Text::styled("Loading Linear issues…", muted)
    } else if linear.connected {
        Text::styled(
            "No assigned Linear tickets.\n\nSelect a ticket when the list contains issues.",
            muted,
        )
    } else {
        Text::styled("Connect Linear to get started", muted)
    };
    if !notice.is_empty() {
        text.lines.push(Line::default());
        text.lines.extend(notice.lines().map(|line| {
            Line::from(Span::styled(
                line.to_owned(),
                Style::default().fg(TICKET_ACCENT),
            ))
        }));
    }
    text
}

pub fn draw_details(
    frame: &mut Frame,
    area: Rect,
    text: Text<'_>,
    focused: bool,
    starting: Option<(&str, std::time::Duration)>,
) {
    let startup_title = starting.map_or_else(String::new, |(agent, elapsed)| {
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let spinner = FRAMES[(elapsed.as_millis() / 100) as usize % FRAMES.len()];
        format!(" Starting {agent}… {spinner} ")
    });
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .title(" Ticket overview ")
                .title(
                    Line::from(Span::styled(
                        startup_title,
                        Style::default().fg(Color::Cyan),
                    ))
                    .right_aligned(),
                )
                .border_style(if focused {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(TICKET_MUTED)
                })
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

pub fn help(frame: &mut Frame, escape: char, editor: &str, pane_navigation: bool) {
    let heading = |label: &str| {
        Line::from(Span::styled(
            format!("  {label}"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let binding = |key: String, action: String| {
        Line::from(vec![
            Span::styled(format!("  {key:<12}"), Style::default().fg(Color::Cyan)),
            Span::raw(action),
        ])
    };
    let mut lines = vec![
        heading("TICKET"),
        binding("a".into(), "Agent · paste if new".into()),
        binding("Enter".into(), "Agent · no paste".into()),
        binding("e".into(), format!("Open {editor}")),
        binding("t".into(), "Open shell".into()),
        binding("o".into(), "Open issue in Linear".into()),
        Line::default(),
        heading("NAVIGATION"),
        binding("j/k · ↑/↓".into(), "Select issue".into()),
        binding("b".into(), "Sidebar / full screen".into()),
        binding(
            format!("Ctrl-{}", escape.to_ascii_uppercase()),
            "Return to manager".into(),
        ),
    ];
    if pane_navigation {
        lines.push(binding(
            "Ctrl-H / L".into(),
            "Sidebar / overview or tool".into(),
        ));
    }
    lines.extend([
        Line::default(),
        heading("LINEAR & APP"),
        binding("l".into(), "Choose Linear workspace".into()),
        binding("r".into(), "Refresh issues".into()),
        binding("s".into(), "Show status".into()),
        binding("q / Ctrl-C".into(), "Quit".into()),
    ]);
    let area = frame.area();
    let width = area.width.min(54);
    let height = area.height.min(lines.len() as u16 + 2);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Keyboard shortcuts ")
                .title_bottom(" Esc / ? close ")
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        popup,
    );
}

pub struct WorkspacePicker<'a> {
    pub repository: &'a str,
    pub current: Option<&'a str>,
    pub available: Option<&'a str>,
    pub is_current: bool,
    pub api_key: bool,
    pub busy: bool,
    pub message: &'a str,
}

pub fn workspace_picker(frame: &mut Frame, picker: WorkspacePicker<'_>) {
    let area = frame.area();
    let width = area.width.min(66);
    let height = area.height.min(19);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let muted = Style::default().fg(Color::DarkGray);
    let heading = |text: &str| {
        Line::from(Span::styled(
            text.to_owned(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let mut lines = vec![
        heading("REPOSITORY"),
        Line::from(picker.repository.to_owned()),
        Line::from(Span::styled(
            format!("Current: {}", picker.current.unwrap_or("Not mapped")),
            muted,
        )),
        Line::default(),
        heading(if picker.api_key {
            "AVAILABLE WITH YOUR API KEY"
        } else {
            "CONNECT A WORKSPACE"
        }),
    ];
    if let Some(name) = picker.available {
        lines.push(
            Line::from(vec![
                Span::styled(
                    " › ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    name.to_owned(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if picker.is_current {
                        "  ✓ current"
                    } else {
                        ""
                    },
                    muted,
                ),
            ])
            .style(Style::default().bg(Color::Rgb(24, 34, 42))),
        );
    } else if !picker.api_key {
        lines.push(Line::from(Span::styled(
            " › Sign in with Linear",
            Style::default().fg(Color::Cyan),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            if picker.busy {
                "   Loading workspace…"
            } else {
                "   No workspace available"
            },
            muted,
        )));
    }
    lines.extend([
        Line::default(),
        Line::from(Span::styled(
            if picker.api_key {
                "An API key gives access to one Linear workspace."
            } else {
                "Your browser will open to choose a workspace."
            },
            muted,
        )),
        Line::default(),
        Line::from(picker.message.to_owned()),
    ]);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .title(" Linear workspace ")
                .title_bottom(Line::from(vec![
                    Span::styled(" Enter", Style::default().fg(Color::Cyan)),
                    Span::raw(" select · "),
                    Span::styled("r", Style::default().fg(Color::Cyan)),
                    Span::raw(" retry · "),
                    Span::styled("Esc", Style::default().fg(Color::Cyan)),
                    Span::raw(" close "),
                ]))
                .border_style(muted)
                .padding(Padding::new(2, 2, 1, 1)),
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
    fn workspace_picker_shows_selection_and_controls_at_multiple_sizes() {
        for (width, height) in [(40, 20), (80, 24), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    workspace_picker(
                        frame,
                        WorkspacePicker {
                            repository: "/forge/exchange",
                            current: Some("Forge"),
                            available: Some("Forge"),
                            is_current: true,
                            api_key: true,
                            busy: false,
                            message: "Enter to use this workspace.",
                        },
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            for label in [
                "Linear workspace",
                "REPOSITORY",
                "/forge/exchange",
                "Current: Forge",
                "AVAILABLE WITH YOUR API KEY",
                "› Forge",
                "✓ current",
                "Enter",
                "retry",
                "Esc",
            ] {
                assert!(
                    text.contains(label),
                    "Missing {label} at {width}x{height}: {text}"
                );
            }
        }
    }

    #[test]
    fn workspace_picker_handles_loading_errors_and_tiny_terminals() {
        for (width, height) in [(1, 1), (20, 6), (66, 19)] {
            for busy in [true, false] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        workspace_picker(
                            frame,
                            WorkspacePicker {
                                repository: "/forge/exchange",
                                current: None,
                                available: None,
                                is_current: false,
                                api_key: true,
                                busy,
                                message: if busy {
                                    "Finding your workspace…"
                                } else {
                                    "Cannot identify the Linear workspace"
                                },
                            },
                        )
                    })
                    .unwrap();
            }
        }
    }

    #[test]
    fn help_keeps_bindings_and_close_hint_visible() {
        for width in [40, 80] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| help(frame, 'i', "Neovim", true))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text = (0..24)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            for label in [
                "Keyboard shortcuts",
                "TICKET",
                "NAVIGATION",
                "LINEAR & APP",
                "Agent · paste if new",
                "Open issue in Linear",
                "Sidebar / full screen",
                "Ctrl-I",
                "Quit",
                "Esc / ? close",
            ] {
                assert!(
                    text.contains(label),
                    "Missing {label} at width {width}: {text}"
                );
            }
        }
    }

    #[test]
    fn draws_without_services_at_narrow_and_wide_sizes() {
        let tickets = Tickets {
            items: &[],
            selected: 0,
            connected: false,
            busy: false,
        };
        assert_eq!(
            details(&tickets, "").to_string(),
            "Connect Linear to get started"
        );
        for width in [0, 40, 100] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| {
                    let content = draw(frame, &tickets, true, true);
                    assert_eq!(content.width, if width >= 70 { width - 28 } else { width });
                    draw_details(frame, content, details(&tickets, ""), false, None);
                })
                .unwrap();
        }
    }
}
