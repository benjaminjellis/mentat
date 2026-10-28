use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};

pub struct Form {
    pub details: Option<linear::Details>,
    pub title: String,
    pub description: String,
    pub comment: String,
    pub state: usize,
    pub priority: u8,
    pub focus: usize,
    pub comments: bool,
    pub message: String,
    pub busy: bool,
    pub load_pending: bool,
    pub dirty: bool,
    pub posting: bool,
    cursor: usize,
    scroll: Option<u16>,
}
impl Default for Form {
    fn default() -> Self {
        Self {
            details: None,
            title: String::new(),
            description: String::new(),
            comment: String::new(),
            state: 0,
            priority: 0,
            focus: 0,
            comments: false,
            message: "Loading issue…".into(),
            busy: false,
            load_pending: false,
            dirty: false,
            posting: false,
            cursor: 0,
            scroll: None,
        }
    }
}
impl Form {
    pub fn loaded(&mut self, mut details: linear::Details, saved: bool) {
        // Retain the original baseline for unsaved fields, so a refresh cannot
        // turn untouched fields into edits that overwrite another user's work.
        if self.dirty
            && (!saved || self.posting)
            && let Some(original) = &self.details
        {
            let comments = std::mem::take(&mut details.comments);
            details = original.clone();
            details.comments = comments;
        }
        if !self.dirty || (saved && !self.posting) {
            self.title = details.issue.title.clone();
            self.description = details.issue.description.clone().unwrap_or_default();
            self.priority = details.priority;
            self.state = details
                .team
                .states
                .nodes
                .iter()
                .position(|s| s.id == details.state.id)
                .unwrap_or(0);
            self.dirty = false;
        }
        if saved && self.posting {
            self.comment.clear();
        }
        self.details = Some(details);
        self.busy = false;
        self.message = if saved { "Saved to Linear." } else { "" }.into();
        self.cursor = self.text().map_or(0, String::len);
    }
    fn text(&self) -> Option<&String> {
        match (self.comments, self.focus) {
            (true, 0) => Some(&self.comment),
            (false, 0) => Some(&self.title),
            (false, 1) => Some(&self.description),
            _ => None,
        }
    }
    fn text_mut(&mut self) -> Option<&mut String> {
        match (self.comments, self.focus) {
            (true, 0) => Some(&mut self.comment),
            (false, 0) => Some(&mut self.title),
            (false, 1) => Some(&mut self.description),
            _ => None,
        }
    }
    pub fn paste(&mut self, text: &str) {
        if self.busy || self.load_pending {
            return;
        }
        let multiline = self.comments || self.focus == 1;
        let text: String = text
            .chars()
            .filter(|c| !c.is_control() || (*c == '\n' && multiline))
            .collect();
        let cursor = self.cursor;
        if let Some(value) = self.text_mut() {
            value.insert_str(cursor, &text);
            self.cursor += text.len();
            if !self.comments {
                self.dirty = true;
            }
        }
    }
    pub fn key(&mut self, key: KeyEvent) -> Option<linear::EditOperation> {
        if self.busy || self.load_pending {
            return None;
        }
        let count = if self.comments { 2 } else { 6 };
        match key.code {
            KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
                self.comments = !self.comments;
                self.focus = 0;
                self.scroll = None;
                self.cursor = self.text().map_or(0, String::len);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = (self.focus
                    + if key.code == KeyCode::Tab {
                        1
                    } else {
                        count - 1
                    })
                    % count;
                self.cursor = self.text().map_or(0, String::len);
                self.scroll = None;
            }
            KeyCode::Down if key.modifiers == KeyModifiers::CONTROL => {
                self.scroll = Some(self.scroll.unwrap_or(0).saturating_add(1));
            }
            KeyCode::Up if key.modifiers == KeyModifiers::CONTROL => {
                self.scroll = Some(self.scroll.unwrap_or(0).saturating_sub(1));
            }
            KeyCode::Left | KeyCode::Right if self.text().is_some() => {
                let value = self.text().unwrap();
                self.cursor = if key.code == KeyCode::Left {
                    value[..self.cursor]
                        .char_indices()
                        .last()
                        .map_or(0, |(i, _)| i)
                } else {
                    value[self.cursor..]
                        .chars()
                        .next()
                        .map_or(self.cursor, |c| self.cursor + c.len_utf8())
                };
            }
            KeyCode::Up | KeyCode::Down if self.text().is_some() => {
                let value = self.text().unwrap();
                let start = value[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
                let column = value[start..self.cursor].chars().count();
                let target = if key.code == KeyCode::Up {
                    if start == 0 {
                        0
                    } else {
                        value[..start - 1].rfind('\n').map_or(0, |i| i + 1)
                    }
                } else {
                    value[self.cursor..]
                        .find('\n')
                        .map_or(value.len(), |i| self.cursor + i + 1)
                };
                let line = value[target..].split('\n').next().unwrap_or("");
                self.cursor = target
                    + line
                        .char_indices()
                        .nth(column)
                        .map_or(line.len(), |(i, _)| i);
            }
            KeyCode::Delete => {
                let cursor = self.cursor;
                if let Some(value) = self.text_mut()
                    && cursor < value.len()
                {
                    value.remove(cursor);
                    if !self.comments {
                        self.dirty = true;
                    }
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text().map_or(0, String::len),
            KeyCode::Backspace if self.cursor > 0 => {
                let cursor = self.cursor;
                if let Some(value) = self.text_mut() {
                    let start = value[..cursor].char_indices().last().unwrap().0;
                    value.replace_range(start..cursor, "");
                    self.cursor = start;
                    if !self.comments {
                        self.dirty = true;
                    }
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.paste(&c.to_string())
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right if !self.comments => {
                let forward = matches!(key.code, KeyCode::Down | KeyCode::Right);
                if self.focus == 2
                    && let Some(d) = &self.details
                {
                    let n = d.team.states.nodes.len();
                    if n > 0 {
                        self.state = (self.state + if forward { 1 } else { n - 1 }) % n;
                        self.dirty = true;
                    }
                }
                if self.focus == 3 {
                    self.priority = (self.priority + if forward { 1 } else { 4 }) % 5;
                    self.dirty = true;
                }
            }
            KeyCode::Enter if self.comments && self.focus == 1 => {
                if self.comment.trim().is_empty() {
                    self.message = "Write a comment first.".into();
                } else {
                    self.posting = true;
                    return Some(linear::EditOperation::Comment(self.comment.clone()));
                }
            }
            KeyCode::Enter if !self.comments && self.focus == 4 => {
                if self.title.trim().is_empty() {
                    self.message = "Title cannot be blank.".into();
                    return None;
                }
                let Some(d) = self.details.as_ref() else {
                    self.message = "Reopen this issue to load its details before saving.".into();
                    return None;
                };
                let mut input = serde_json::Map::new();
                if self.title != d.issue.title {
                    input.insert("title".into(), self.title.clone().into());
                }
                if self.description != d.issue.description.clone().unwrap_or_default() {
                    input.insert("description".into(), self.description.clone().into());
                }
                if self.priority != d.priority {
                    input.insert("priority".into(), self.priority.into());
                }
                if let Some(state) = d.team.states.nodes.get(self.state)
                    && state.id != d.state.id
                {
                    input.insert("stateId".into(), state.id.clone().into());
                }
                if input.is_empty() {
                    self.message = "No changes to save.".into();
                    return None;
                }
                self.posting = false;
                return Some(linear::EditOperation::Save(input.into()));
            }
            KeyCode::Enter if !self.comments && self.focus == 5 => {
                if let Some(d) = self.details.clone() {
                    self.dirty = false;
                    self.loaded(d, false);
                    self.message = "Changes discarded.".into();
                }
            }
            KeyCode::Enter if self.comments || self.focus == 1 => self.paste("\n"),
            _ => {}
        }
        None
    }
    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(" Linear issue ")
            .border_style(Style::default().fg(Color::Cyan))
            .padding(ratatui::widgets::Padding::horizontal(2));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 3 {
            return;
        }
        let accent = Style::default().fg(Color::Cyan);
        let mut lines = vec![
            Line::from(vec![Span::styled(
                if self.comments {
                    "  Details    [Comments]"
                } else {
                    "  [Details]    Comments"
                },
                accent,
            )]),
            Line::default(),
        ];
        let mut focused_line = 0;
        if self.comments
            && let Some(d) = &self.details
        {
            for c in &d.comments {
                lines.push(Line::styled(
                    format!(
                        "{}  ·  {}",
                        c.user.as_ref().map_or("Unknown", |u| &u.name),
                        c.created_at
                    ),
                    Style::default().fg(Color::DarkGray),
                ));
                lines.extend(c.body.lines().map(|l| Line::raw(l.to_owned())));
                lines.push(Line::default());
            }
        }
        let state = self
            .details
            .as_ref()
            .and_then(|d| d.team.states.nodes.get(self.state))
            .map_or("", |s| s.name.as_str());
        let priority =
            ["No priority", "Urgent", "High", "Medium", "Low"][usize::from(self.priority.min(4))];
        let fields: Vec<(&str, &str)> = if self.comments {
            vec![("New comment", &self.comment), ("", "Post comment")]
        } else {
            vec![
                ("Title", &self.title),
                ("Description", &self.description),
                ("Status", state),
                ("Priority", priority),
                ("", "Save changes"),
                ("", "Cancel"),
            ]
        };
        for (i, (label, value)) in fields.iter().enumerate() {
            let active = i == self.focus;
            if active {
                focused_line = lines
                    .iter()
                    .map(|l| l.width().max(1).div_ceil(inner.width.max(1) as usize))
                    .sum();
            }
            let style = if active {
                accent.add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            if label.is_empty() {
                lines.push(Line::styled(
                    format!("{} [ {} ]", if active { "›" } else { " " }, value),
                    style,
                ));
                lines.push(Line::default());
                continue;
            }
            if !self.comments && matches!(i, 2 | 3) {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} {:<12}", if active { "›" } else { " " }, label),
                        style,
                    ),
                    Span::raw(format!("‹ {} ›", value)),
                ]));
                lines.push(Line::default());
                continue;
            }
            lines.push(Line::styled(
                format!("{} {}", if active { "›" } else { " " }, label),
                style,
            ));
            let mut display = value.to_string();
            if active && self.text().is_some() {
                let before = &display[..self.cursor.min(display.len())];
                focused_line += 1 + before
                    .split('\n')
                    .map(|l| {
                        (l.chars().count() + 2)
                            .max(1)
                            .div_ceil(inner.width.max(1) as usize)
                    })
                    .sum::<usize>()
                    .saturating_sub(1);
                display.insert(self.cursor.min(display.len()), '▏');
            }
            lines.extend(display.lines().map(|s| Line::raw(format!("  {s}"))));
            lines.push(Line::default());
        }
        let body = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(2),
        );
        let scroll = self.scroll.unwrap_or_else(|| {
            focused_line
                .saturating_sub(body.height.saturating_sub(4) as usize)
                .min(u16::MAX as usize) as u16
        });
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            body,
        );
        let hint = if inner.width >= 42 {
            "Esc back · Tab focus · Ctrl-N tabs · Ctrl-↑↓ scroll"
        } else {
            "Esc back · Tab · Ctrl-N"
        };
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[test]
    fn unicode_editing_and_paste_do_not_trigger_shortcuts() {
        let mut form = Form::default();
        form.paste("héllo\u{1b}\n");
        assert_eq!(form.title, "héllo");
        form.key(key(KeyCode::Home));
        form.key(key(KeyCode::Right));
        form.key(key(KeyCode::Right));
        form.key(key(KeyCode::Backspace));
        assert_eq!(form.title, "hllo");
        form.key(key(KeyCode::Char('q')));
        assert_eq!(form.title, "hqllo");
    }
    #[test]
    fn comments_have_independent_submission_and_busy_guard() {
        let mut form = Form::default();
        form.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        form.paste("First\nSecond");
        form.key(key(KeyCode::Tab));
        assert!(
            matches!(form.key(key(KeyCode::Enter)),Some(linear::EditOperation::Comment(body)) if body == "First\nSecond")
        );
        form.busy = true;
        assert!(form.key(key(KeyCode::Enter)).is_none());
        assert_eq!(form.comment, "First\nSecond");
        assert!(form.title.is_empty());
    }
    #[test]
    fn form_renders_actions_in_narrow_terminal() {
        let form = Form {
            focus: 4,
            ..Form::default()
        };
        let backend = ratatui::backend::TestBackend::new(48, 18);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| form.draw(f, f.area())).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("Save changes"));
        assert!(screen.contains("Esc back"));
        assert!(!screen.contains(&form.message));
    }
}
