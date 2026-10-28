mod terminal_view;
mod text_selection;

pub use terminal_view::{TerminalSelection, TerminalView};
pub use text_selection::TextSelection;

use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, List, ListItem, ListState, Padding, Paragraph, Widget, Wrap},
};
use std::time::Duration;

pub struct Ticket<'a> {
    pub identifier: &'a str,
    pub has_workspace: bool,
    pub title: &'a str,
    pub status: &'a str,
    pub team: &'a str,
    pub url: &'a str,
    pub description: Option<&'a str>,
}

pub struct Tickets<'a> {
    pub items: &'a [Ticket<'a>],
    pub selected: usize,
    /// Elapsed time in the current Linear operation, which also drives the spinner.
    pub busy: Option<Duration>,
    pub search: Option<Search<'a>>,
    pub status_filter: Option<&'a str>,
}

pub struct Search<'a> {
    pub query: &'a str,
    pub active: bool,
}

impl<'a> Tickets<'a> {
    fn issue(&self) -> Option<&'a Ticket<'a>> {
        self.items.get(self.selected)
    }
}

const TICKET_ACCENT: Color = Color::Rgb(193, 163, 113);
const TICKET_TEXT: Color = Color::Rgb(224, 215, 198);
const TICKET_MUTED: Color = Color::Rgb(145, 143, 133);
const TICKET_STATUS: Color = Color::Rgb(161, 182, 151);
const TICKET_SELECTED: Color = Color::Rgb(57, 53, 45);

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The caller owns the clock, so a hidden frame counter cannot desynchronise views.
fn spinner(elapsed: Duration) -> char {
    SPINNER[(elapsed.as_millis() / 100) as usize % SPINNER.len()]
}

/// Centre a popup over the whole frame and clear whatever it covers. `Length`
/// clamps to the available area, so popups stay inside tiny terminals.
fn popup(frame: &mut Frame, width: u16, height: u16) -> Rect {
    let area = frame
        .area()
        .centered(Constraint::Length(width), Constraint::Length(height));
    frame.render_widget(Clear, area);
    area
}

fn heading(label: &str) -> Line<'_> {
    Line::from(Span::styled(
        label,
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ))
}

pub struct Areas {
    pub sidebar: Rect,
    pub activity: Rect,
    pub content: Rect,
    pub header: Rect,
}

pub fn areas(area: Rect, sidebar_visible: bool) -> Areas {
    if !sidebar_visible {
        return Areas {
            sidebar: Rect::default(),
            activity: Rect::default(),
            content: area,
            header: Rect::default(),
        };
    }
    let sidebar_width = if area.width >= 70 { 28 } else { 0 };
    let header_height = area.height.min(1);
    let height = area.height.saturating_sub(header_height);
    let content_y = area.y + header_height;
    let (sidebar, activity) = if sidebar_width > 0 {
        sidebar_areas(Rect::new(area.x, content_y, sidebar_width, height))
    } else {
        (Rect::default(), Rect::default())
    };
    Areas {
        sidebar,
        activity,
        content: Rect::new(
            area.x + sidebar_width,
            content_y,
            area.width.saturating_sub(sidebar_width),
            height,
        ),
        header: Rect::new(area.x, area.y, area.width, header_height),
    }
}

pub fn draw(
    frame: &mut Frame,
    linear: &Tickets<'_>,
    root_dir: &str,
    focused: bool,
    sidebar_visible: bool,
) -> Rect {
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
            Span::raw("  "),
            Span::styled(root_dir, Style::default().fg(TICKET_MUTED)),
        ])),
        areas.header,
    );
    if areas.sidebar.width == 0 {
        if let Some(search) = &linear.search {
            draw_search(frame, areas.header, search);
        } else if let Some(status) = linear.status_filter {
            frame.render_widget(Paragraph::new(format!("Status: {status}")), areas.header);
        }
    } else {
        let refresh_title = linear
            .busy
            .map_or_else(String::new, |elapsed| format!(" {} ", spinner(elapsed)));
        let block = Block::bordered()
            .title(" Workspace ")
            .title(Line::from(refresh_title).right_aligned())
            .title_bottom(match &linear.search {
                Some(search) if search.active => " Enter apply · Esc clear ",
                Some(_) => " / edit · Esc clear ",
                None => " / search · S status ",
            })
            .border_style(if focused {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            });
        let mut inner = block.inner(areas.sidebar);
        frame.render_widget(block, areas.sidebar);
        if let Some(status) = linear.status_filter {
            let height = inner.height.min(1);
            frame.render_widget(
                Paragraph::new(format!("Status: {status}"))
                    .style(Style::default().fg(TICKET_STATUS)),
                Rect::new(inner.x, inner.y, inner.width, height),
            );
            inner.y += height;
            inner.height -= height;
        }
        if let Some(search) = &linear.search {
            let height = inner.height.min(1);
            draw_search(
                frame,
                Rect::new(inner.x, inner.y, inner.width, height),
                search,
            );
            inner.y += height;
            inner.height -= height;
        }
        if linear.items.is_empty() && (linear.search.is_some() || linear.status_filter.is_some()) {
            frame.render_widget(
                Paragraph::new("No matching tickets.").style(Style::default().fg(TICKET_MUTED)),
                inner,
            );
        }
        let items: Vec<_> = linear
            .items
            .iter()
            .map(|issue| {
                let mut identifier = Line::from(Span::styled(
                    issue.identifier,
                    Style::default()
                        .fg(TICKET_ACCENT)
                        .add_modifier(Modifier::BOLD),
                ));
                if issue.has_workspace {
                    // Leave room for the selection arrow, a gap, and the dot.
                    let width = usize::from(inner.width.saturating_sub(2));
                    let mut text = String::new();
                    for c in issue.identifier.chars() {
                        let next = format!("{text}{c}");
                        if Line::raw(next.clone()).width() > width.saturating_sub(2) {
                            break;
                        }
                        text = next;
                    }
                    identifier.spans[0].content = text.into();
                    identifier.push_span(" ".repeat(width.saturating_sub(identifier.width() + 1)));
                    identifier.push_span(Span::styled("●", Style::default().fg(Color::LightGreen)));
                }
                ListItem::new(vec![
                    identifier,
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

fn draw_search(frame: &mut Frame, area: Rect, search: &Search<'_>) {
    if area.is_empty() {
        return;
    }
    let text = Line::raw(format!("/ {}", search.query));
    let width = text.width();
    let offset = width.saturating_sub(usize::from(area.width.saturating_sub(1)));
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(if search.active {
                Color::Cyan
            } else {
                TICKET_MUTED
            }))
            .scroll((0, offset.min(usize::from(u16::MAX)) as u16)),
        area,
    );
    if search.active {
        frame.set_cursor_position((area.x + (width - offset) as u16, area.y));
    }
}

pub fn details<'a>(issue: Option<&Ticket<'a>>) -> Text<'a> {
    let muted = Style::default().fg(TICKET_MUTED);
    if let Some(issue) = issue {
        let mut lines = vec![
            Line::from(Span::styled(
                issue.identifier,
                Style::default()
                    .fg(TICKET_ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                issue.title,
                Style::default()
                    .fg(TICKET_TEXT)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::default(),
            Line::from(vec![
                Span::styled("Status  ", muted),
                Span::styled(issue.status, Style::default().fg(TICKET_STATUS)),
            ]),
            Line::from(vec![
                Span::styled("Team    ", muted),
                Span::styled(issue.team, Style::default().fg(TICKET_TEXT)),
            ]),
            Line::from(Span::styled(
                issue.url,
                Style::default()
                    .fg(Color::Rgb(151, 169, 177))
                    .add_modifier(Modifier::UNDERLINED),
            )),
            Line::default(),
        ];
        lines.extend(
            issue
                .description
                .unwrap_or_default()
                .lines()
                .map(|line| Line::from(Span::styled(line, Style::default().fg(TICKET_TEXT)))),
        );
        Text::from(lines)
    } else {
        Text::default()
    }
}

pub struct Comment<'a> {
    pub author: &'a str,
    pub created_at: &'a str,
    pub body: &'a str,
}

pub fn append_comments(text: &mut Text<'_>, comments: &[Comment<'_>]) {
    text.lines.push(Line::default());
    text.lines.push(Line::styled(
        format!("COMMENTS ({})", comments.len()),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    text.lines.push(Line::default());
    for comment in comments {
        text.lines.push(Line::from(vec![
            Span::styled(comment.author.to_owned(), Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("  ·  {}", comment.created_at),
                Style::default().fg(TICKET_MUTED),
            ),
        ]));
        text.lines
            .extend(comment.body.lines().map(|line| Line::raw(line.to_owned())));
        text.lines.push(Line::default());
    }
}

fn details_block(focused: bool) -> Block<'static> {
    Block::bordered()
        .title(" Ticket overview ")
        .border_style(if focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TICKET_MUTED)
        })
        .padding(Padding::horizontal(1))
}

pub fn details_area(area: Rect) -> Rect {
    details_block(false).inner(area)
}

pub fn select_details(
    text: Text<'_>,
    area: Rect,
    scroll: u16,
    row: u16,
    col: u16,
) -> TextSelection {
    let area = Rect::new(0, 0, area.width, area.height);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    Paragraph::new(text)
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false })
        .render(area, &mut buffer);
    TextSelection::new(buffer, row, col)
}

pub fn draw_details(
    frame: &mut Frame,
    area: Rect,
    text: Text<'_>,
    focused: bool,
    scroll: u16,
    selection: Option<&TextSelection>,
) {
    let block = details_block(focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some(selection) = selection {
        frame.render_widget(selection, inner);
    } else {
        frame.render_widget(
            Paragraph::new(text)
                .scroll((scroll, 0))
                .wrap(Wrap { trim: false }),
            inner,
        );
    }
}

pub fn help(frame: &mut Frame, escape: char, editor: &str, pane_navigation: bool) {
    let binding = |key: String, action: String| {
        Line::from(vec![
            Span::styled(format!("  {key:<12}"), Style::default().fg(Color::Cyan)),
            Span::raw(action),
        ])
    };
    let mut lines = vec![
        heading("  TICKET"),
        binding("i, then e".into(), "Open ticket, then edit".into()),
        binding("a".into(), "Agent".into()),
        binding("Enter".into(), "Agent · send ticket".into()),
        binding("e".into(), format!("Open {editor}")),
        binding("t".into(), "Open shell".into()),
        binding("o".into(), "Open issue in Linear".into()),
        binding("b".into(), "Linear bookmark (jj)".into()),
        binding("d".into(), "Linear description (jj)".into()),
        binding("p".into(), "Push (jj git push)".into()),
        heading("  NAVIGATION"),
        binding("j/k · ↑/↓".into(), "Select · / search".into()),
        binding("f".into(), "Sidebar / full screen".into()),
        binding("Ctrl-C/V".into(), "Copy selection / paste".into()),
        binding(
            format!("Ctrl-{}", escape.to_ascii_uppercase()),
            "Return to manager".into(),
        ),
    ];
    if pane_navigation {
        lines.push(binding(
            "Ctrl-H / L".into(),
            "Pane focus (manager only)".into(),
        ));
    }
    lines.extend([
        heading("  LINEAR & APP"),
        binding("l".into(), "Choose Linear workspace".into()),
        binding("r".into(), "Refresh issues".into()),
        binding("s / S".into(), "App status / filter".into()),
        binding("q".into(), "Quit".into()),
    ]);
    let area = popup(frame, 54, lines.len() as u16 + 2);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Keyboard shortcuts ")
                .title_bottom(" Esc / ? close ")
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        area,
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
    let muted = Style::default().fg(Color::DarkGray);
    let mut lines = vec![
        heading("REPOSITORY"),
        Line::from(picker.repository),
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
        let accent = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        lines.push(
            Line::from(vec![
                Span::styled(" › ", accent),
                Span::styled(name, accent),
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
    } else if picker.api_key {
        lines.push(Line::from(Span::styled(
            if picker.busy {
                "   Loading workspace…"
            } else {
                "   No workspace available"
            },
            muted,
        )));
    } else {
        lines.push(Line::from(Span::styled(
            " › Sign in with Linear",
            Style::default().fg(Color::Cyan),
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
        Line::from(picker.message),
    ]);
    let area = popup(frame, 66, 19);
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
        area,
    );
}

pub fn status_filter(frame: &mut Frame, statuses: &[String], selected: usize) {
    let height = statuses.len().saturating_add(3).min(18) as u16;
    let area = popup(frame, 48, height);
    let items = std::iter::once(ListItem::new("All statuses"))
        .chain(statuses.iter().map(|status| ListItem::new(status.as_str())));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::bordered()
                    .title(" Filter by status ")
                    .title_bottom(" ↑↓ select · Enter apply · Esc cancel ")
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .highlight_symbol("› ")
            .highlight_style(
                Style::default()
                    .bg(TICKET_SELECTED)
                    .add_modifier(Modifier::BOLD),
            ),
        area,
        &mut state,
    );
}

pub fn status_popup(frame: &mut Frame, text: String) {
    let area = popup(frame, 76, 18);
    frame.render_widget(
        Paragraph::new(format!("{text}\n\nEsc or s closes this popup."))
            .wrap(Wrap { trim: false })
            .block(
                Block::bordered()
                    .title(" Status ")
                    .border_style(Style::default().fg(Color::Cyan)),
            ),
        area,
    );
}

/// Draw a borrowed terminal screen without owning its session or lock.
pub fn draw_terminal(frame: &mut Frame, area: Rect, screen: &vt100::Screen, show_cursor: bool) {
    frame.render_widget(TerminalView(screen), area);
    let (row, col) = screen.cursor_position();
    if show_cursor
        && screen.scrollback() == 0
        && !screen.hide_cursor()
        && row < area.height
        && col < area.width
    {
        frame.set_cursor_position((area.x + col, area.y + row));
    }
}

pub fn quit_popup(frame: &mut Frame) {
    let area = popup(frame, 64, 9);
    frame.render_widget(Paragraph::new("An editor or shell is still open.\n\nForce quit loses unsaved buffers and stops shell jobs.\nWorkspace files and Agent session IDs remain saved.\n\nf: Force quit    Esc: Return and close tools yourself")
        .wrap(Wrap { trim: false }).block(Block::bordered().title(" Quit Mentat ")), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn workspace_dot_preserves_alignment_and_selection() {
        let items = [true, false].map(|has_workspace| Ticket {
            identifier: "APP-1",
            has_workspace,
            title: "Example ticket",
            status: "Todo",
            team: "App",
            url: "",
            description: None,
        });
        for selected in 0..items.len() {
            let tickets = Tickets {
                items: &items,
                selected,
                busy: None,
                search: None,
                status_filter: None,
            };
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal
                .draw(|frame| {
                    draw(frame, &tickets, "/project", true, true);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            for (index, item) in items.iter().enumerate() {
                let y = 2 + index as u16 * 2;
                let row = (1..8).map(|x| buffer[(x, y)].symbol()).collect::<String>();
                assert_eq!(
                    row,
                    format!("{}APP-1", if index == selected { "› " } else { "  " },)
                );
                assert_eq!(
                    buffer[(26, y)].symbol(),
                    if item.has_workspace { "●" } else { " " }
                );
                if item.has_workspace {
                    assert_eq!(buffer[(26, y)].fg, Color::LightGreen);
                    assert_ne!(buffer[(26, y)].fg, buffer[(3, y)].fg);
                }
                if index == selected {
                    assert_eq!(buffer[(26, y)].bg, TICKET_SELECTED);
                }
            }
        }
    }

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
                .draw(|frame| help(frame, 'g', "Neovim", true))
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
                "Agent · send ticket",
                "Open ticket, then edit",
                "Open issue in Linear",
                "Linear bookmark (jj)",
                "Linear description (jj)",
                "Push (jj git push)",
                "Sidebar / full screen",
                "Ctrl-G",
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
            busy: None,
            search: None,
            status_filter: None,
        };
        assert_eq!(details(None).to_string(), "");
        for width in [0, 40, 100] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| {
                    let content = draw(frame, &tickets, "/home/user/project", true, true);
                    assert_eq!(content.width, if width >= 70 { width - 28 } else { width });
                    draw_details(frame, content, details(None), false, 0, None);
                })
                .unwrap();
        }
    }
}

fn sidebar_areas(area: Rect) -> (Rect, Rect) {
    let height = if area.height < 12 {
        area.height.min(1)
    } else {
        (area.height / 4).clamp(3, 8)
    };
    (
        Rect::new(
            area.x,
            area.y,
            area.width,
            area.height.saturating_sub(height),
        ),
        Rect::new(
            area.x,
            area.bottom().saturating_sub(height),
            area.width,
            height,
        ),
    )
}
pub fn draw_activity(
    frame: &mut Frame,
    area: Rect,
    events: &[String],
    scroll: usize,
    starting: Option<(&str, Duration)>,
) {
    let startup =
        starting.map(|(agent, elapsed)| format!("Starting {agent}… {}", spinner(elapsed)));
    let end = events
        .len()
        .saturating_sub(scroll.min(events.len().saturating_sub(1)));
    if area.height <= 1 {
        frame.render_widget(
            Paragraph::new(
                startup
                    .as_deref()
                    .or_else(|| events.get(end.saturating_sub(1)).map(String::as_str))
                    .unwrap_or("No activity yet"),
            )
            .style(Style::default().fg(TICKET_MUTED)),
            area,
        );
        return;
    }
    let block = Block::bordered()
        .title(" Activity ")
        .border_style(Style::default().fg(TICKET_MUTED))
        .padding(Padding::horizontal(1));
    let count = area.height.saturating_sub(2) as usize;
    let width = block.inner(area).width.max(1) as usize;
    let mut wrapped = Vec::new();
    for event in events[..end]
        .iter()
        .map(String::as_str)
        .chain(startup.as_deref())
    {
        let (timestamp, message) = event.split_once("  ").unwrap_or(("", event));
        if !timestamp.is_empty() {
            wrapped.push(timestamp.to_owned());
        }
        let mut line = String::new();
        for word in message.split_whitespace() {
            if !line.is_empty() && Line::raw(format!("{line} {word}")).width() > width {
                wrapped.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            for c in word.chars() {
                if Line::raw(format!("{line}{c}")).width() > width && !line.is_empty() {
                    wrapped.push(std::mem::take(&mut line));
                }
                line.push(c);
            }
        }
        if !line.is_empty() {
            wrapped.push(line);
        }
    }
    if wrapped.is_empty() {
        wrapped.push("No activity yet".into());
    }
    let lines: Vec<Line> = wrapped[wrapped.len().saturating_sub(count)..]
        .iter()
        .map(|s| Line::raw(s.as_str()))
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(TICKET_MUTED))
            .block(block),
        area,
    );
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    #[test]
    fn comments_show_only_linear_content() {
        let mut text = Text::default();
        append_comments(
            &mut text,
            &[Comment {
                author: "Sam",
                created_at: "2026-10-14",
                body: "First line\nSecond line",
            }],
        );
        let rendered = text.to_string();
        for expected in [
            "COMMENTS (1)",
            "Sam",
            "2026-10-14",
            "First line",
            "Second line",
        ] {
            assert!(rendered.contains(expected));
        }
        let mut empty = Text::default();
        append_comments(&mut empty, &[]);
        assert_eq!(empty.to_string().trim(), "COMMENTS (0)");
    }
    #[test]
    fn activity_is_left_of_full_height_content_and_hides_with_sidebar() {
        let layout = areas(Rect::new(0, 0, 100, 30), true);
        assert_eq!(layout.activity.x, 0);
        assert_eq!(layout.sidebar.bottom(), layout.activity.y);
        assert_eq!(layout.activity.right(), layout.content.x);
        assert_eq!(layout.content.height, 29);
        assert_eq!(
            areas(Rect::new(0, 0, 100, 30), false).activity,
            Rect::default()
        );
    }
    #[test]
    fn sidebar_split_preserves_bounds_at_all_sizes() {
        for height in 0..80 {
            let area = Rect::new(0, 1, 28, height);
            let (ticket, activity) = sidebar_areas(area);
            assert_eq!(ticket.bottom(), activity.y);
            assert_eq!(activity.bottom(), area.bottom());
            assert_eq!(activity.width, ticket.width);
            assert!(activity.height <= 8);
        }
    }
}
