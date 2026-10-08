use crate::{
    agent_session, config::Config, linear_state::Linear, settings, ticket::PendingPaste,
    workspace::Workspaces,
};
use ::terminal::{input, pty::Session};
use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::Show,
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, IsTerminal},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

type Host = Terminal<CrosstermBackend<io::Stdout>>;

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self> {
        let guard = Self;
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Manager,
    Primary,
    Editor,
    Shell,
}

struct TicketSession {
    config: Config,
    primary: Option<Session>,
    editor: Option<Session>,
    shell: Option<Session>,
    last: View,
    paste: Option<PendingPaste>,
    _lock: File,
}
impl TicketSession {
    fn child(&self, view: View) -> Option<&Session> {
        match view {
            View::Primary => self.primary.as_ref(),
            View::Editor => self.editor.as_ref(),
            View::Shell => self.shell.as_ref(),
            View::Manager => None,
        }
    }
    fn has_children(&self) -> bool {
        self.primary.is_some() || self.editor.is_some() || self.shell.is_some()
    }
    fn protected(&self) -> bool {
        self.editor.is_some()
            || self.shell.is_some()
            || (self.config.protect_child && self.primary.is_some())
    }
    fn shutdown(&mut self) -> Result<()> {
        let mut error = None;
        for child in [&mut self.primary, &mut self.editor, &mut self.shell] {
            if let Some(mut child) = child.take()
                && let Err(e) = child.shutdown()
            {
                error.get_or_insert(e);
            }
        }
        error.map_or(Ok(()), Err)
    }
}

struct App {
    config: Config,
    workspaces: Workspaces,
    sessions: HashMap<String, TicketSession>,
    current: Option<String>,
    linear: Linear,
    view: View,
    help: bool,
    status: bool,
    quit_prompt: bool,
    notice: String,
    next_refresh: Instant,
    next_cleanup: Instant,
}

pub fn run(config: Config) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("Mentat needs an interactive terminal");
    }
    let workspaces = Workspaces::open(&config.cwd, &settings::state_directory()?)?;
    let tracked = workspaces
        .list()?
        .into_iter()
        .map(|w| w.ticket_id)
        .collect();
    let current = workspaces.selected()?;
    let mut linear = Linear::new(config.linear.clone(), tracked);
    if let Some(id) = &current
        && let Some(index) = linear
            .snapshot
            .as_ref()
            .and_then(|s| s.issues.iter().position(|i| &i.id == id))
    {
        linear.selected = index;
    }
    linear.restore_selection = current.clone();
    let mut app = App {
        config,
        workspaces,
        sessions: HashMap::new(),
        current,
        linear,
        view: View::Manager,
        help: false,
        status: false,
        quit_prompt: false,
        notice: String::new(),
        next_refresh: Instant::now() + Duration::from_secs(60),
        next_cleanup: Instant::now(),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let mut signals = Vec::new();
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        signals.push(signal_hook::flag::register(signal, stop.clone())?);
    }
    let old_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        drop(TerminalGuard);
        old_hook(info);
    }));
    let _guard = TerminalGuard::enter()?;
    let mut host = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = app.event_loop(&mut host, &stop);
    let mut shutdown = Ok(());
    for session in app.sessions.values_mut() {
        if let Err(error) = session.shutdown() {
            shutdown = Err(error);
        }
    }
    for signal in signals {
        signal_hook::low_level::unregister(signal);
    }
    result.and(shutdown)
}

impl App {
    fn session(&self) -> Option<&TicketSession> {
        self.current.as_ref().and_then(|id| self.sessions.get(id))
    }

    fn open_ticket(&mut self, view: View, paste: bool, size: (u16, u16)) -> Result<()> {
        let issue = self
            .linear
            .issue()
            .cloned()
            .context("Select a Linear ticket first")?;
        self.workspaces.select(&issue.id)?;
        self.current = Some(issue.id.clone());
        if !self.sessions.contains_key(&issue.id) {
            let record = self.workspaces.ensure(&issue.id)?;
            let lock = self.workspaces.lock(&record)?;
            let mut config = self.config.clone();
            config.cwd = record.path;
            if config.resume.is_none() && !config.new_conversation {
                config.resume = self
                    .workspaces
                    .conversation(&issue.id, config.agent.name())?;
            }
            self.sessions.insert(
                issue.id.clone(),
                TicketSession {
                    config,
                    primary: None,
                    editor: None,
                    shell: None,
                    last: View::Primary,
                    paste: None,
                    _lock: lock,
                },
            );
            self.linear.tracked = self
                .workspaces
                .list()?
                .into_iter()
                .map(|w| w.ticket_id)
                .collect();
        }
        let session = self
            .sessions
            .get_mut(&issue.id)
            .expect("ticket session exists");
        let area = ui::areas(ratatui::layout::Rect::new(0, 0, size.0, size.1)).content;
        match view {
            View::Primary => {
                if session.primary.is_none() {
                    agent_session::prepare(&mut session.config)?;
                    if let Some(id) = &session.config.resume {
                        self.workspaces.save_conversation(
                            &issue.id,
                            session.config.agent.name(),
                            id,
                        )?;
                    }
                    session.primary = Some(Session::spawn(
                        &session.config.command,
                        &session.config.cwd,
                        area.width,
                        area.height,
                    )?);
                    session.config.new_conversation = false;
                    self.config.resume = None;
                    self.config.new_conversation = false;
                }
                if paste && session.config.agent_mode {
                    session.paste = Some(PendingPaste::new(&issue));
                }
            }
            View::Editor => {
                if session.editor.is_none() {
                    session.editor = Some(Session::spawn(
                        &session.config.editor.command(),
                        &session.config.cwd,
                        area.width,
                        area.height,
                    )?);
                }
            }
            View::Shell => {
                if session.shell.is_none() {
                    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
                    session.shell = Some(Session::spawn(
                        &[shell],
                        &session.config.cwd,
                        area.width,
                        area.height,
                    )?);
                }
            }
            View::Manager => {}
        }
        session.last = view;
        self.view = view;
        self.notice.clear();
        Ok(())
    }

    fn poll(&mut self) -> Result<bool> {
        let mut redraw = self.linear.poll();
        if !self.linear.closed.is_empty() {
            self.workspaces
                .mark_closed(&std::mem::take(&mut self.linear.closed))?;
            self.next_cleanup = Instant::now();
        }
        if Instant::now() >= self.next_refresh {
            self.linear.start(false);
            self.next_refresh = Instant::now() + Duration::from_secs(60);
        }
        for (id, session) in &mut self.sessions {
            let visible = self.current.as_ref() == Some(id);
            for (view, slot) in [
                (View::Primary, &mut session.primary),
                (View::Editor, &mut session.editor),
                (View::Shell, &mut session.shell),
            ] {
                if let Some(child) = slot {
                    if child.poll_exit()? {
                        child.shutdown()?;
                        *slot = None;
                        if visible && self.view == view {
                            self.view = View::Manager;
                            self.notice = "Terminal exited. Open it again to restart.".into();
                            redraw = true;
                        }
                    } else {
                        redraw |= child.changed.swap(false, Ordering::Relaxed)
                            && visible
                            && self.view == view;
                    }
                }
            }
            if !visible || self.view != View::Primary || session.primary.is_none() {
                session.paste = None;
            } else if let (Some(paste), Some(primary)) = (&session.paste, &session.primary) {
                match paste.poll(primary) {
                    Ok(true) => session.paste = None,
                    Ok(false) => {}
                    Err(error) => {
                        session.paste = None;
                        self.notice = error.to_string();
                        self.view = View::Manager;
                        redraw = true;
                    }
                }
            }
        }
        if Instant::now() >= self.next_cleanup {
            for record in self.workspaces.list()?.into_iter().filter(|w| w.cleanup) {
                if self
                    .sessions
                    .get(&record.ticket_id)
                    .is_some_and(TicketSession::has_children)
                {
                    self.notice =
                        "Closed ticket workspace retained until its terminals exit.".into();
                    redraw = true;
                    continue;
                }
                self.sessions.remove(&record.ticket_id);
                match self.workspaces.cleanup(&record) {
                    Ok(true) => {
                        self.notice = "Closed ticket workspace removed.".into();
                        self.linear.tracked.retain(|id| id != &record.ticket_id);
                    }
                    Ok(false) => {
                        self.notice =
                            "Closed ticket workspace retained because it has local changes.".into()
                    }
                    Err(error) => self.notice = format!("Workspace cleanup deferred: {error:#}"),
                }
                redraw = true;
            }
            self.next_cleanup = Instant::now() + Duration::from_secs(5);
        }
        Ok(redraw)
    }

    fn draw(&self, host: &mut Host) -> Result<()> {
        host.draw(|frame| {
            let tickets: Vec<_> = self
                .linear
                .snapshot
                .as_ref()
                .map(|s| {
                    s.issues
                        .iter()
                        .map(|i| ui::Ticket {
                            identifier: &i.identifier,
                            title: &i.title,
                            status: &i.state.name,
                            team: &i.team.name,
                            url: &i.url,
                            description: i.description.as_deref(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let display = ui::Tickets {
                items: &tickets,
                selected: self.linear.selected,
                connected: self.linear.snapshot.is_some(),
                busy: self.linear.busy,
            };
            let content = ui::draw(frame, &display, self.view == View::Manager);
            if let Some(child) = self.session().and_then(|s| s.child(self.view)) {
                let parser = child.parser.lock().unwrap_or_else(|e| e.into_inner());
                ui::draw_terminal(
                    frame,
                    content,
                    parser.screen(),
                    !self.help && !self.status && !self.quit_prompt,
                );
            } else {
                ui::draw_details(frame, content, ui::details(&display, &self.notice));
            }
            if self.help {
                ui::help(
                    frame,
                    self.config.escape,
                    self.config.editor.name(),
                    self.config.pane_navigation,
                );
            }
            if self.status {
                let path = self
                    .session()
                    .map_or(self.workspaces.source.as_path(), |s| s.config.cwd.as_path());
                ui::status_popup(
                    frame,
                    format!(
                        "{}\n\nDirectory: {}\n\n{}",
                        self.linear.status,
                        path.display(),
                        self.notice
                    ),
                );
            }
            if self.quit_prompt {
                ui::quit_popup(frame);
            }
        })?;
        Ok(())
    }

    fn request_quit(&mut self) -> bool {
        if self.sessions.values().any(TicketSession::protected) {
            self.quit_prompt = true;
            false
        } else {
            true
        }
    }

    fn key(&mut self, key: KeyEvent, size: (u16, u16)) -> Result<bool> {
        if self.quit_prompt {
            match key.code {
                KeyCode::Char('f' | 'F') => return Ok(true),
                KeyCode::Esc => self.quit_prompt = false,
                _ => {}
            }
        } else if self.status {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('s')) {
                self.status = false;
            }
        } else if self.help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.help = false;
            }
        } else if self.config.pane_navigation
            && key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('h' | 'j' | 'k' | 'l'))
        {
            match key.code {
                KeyCode::Char('h') => self.view = View::Manager,
                KeyCode::Char('l') => {
                    if let Some(session) = self.session()
                        && session.child(session.last).is_some()
                    {
                        self.view = session.last;
                    }
                }
                _ => {}
            }
        } else if self.view != View::Manager {
            if self.config.is_escape(key) {
                self.view = View::Manager;
            } else if let Some(child) = self.session().and_then(|s| s.child(self.view)) {
                let cursor = child
                    .parser
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .screen()
                    .application_cursor();
                child.write(&input::encode(key, cursor))?;
            }
        } else if key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL)
        {
            return Ok(self.request_quit());
        } else {
            match key.code {
                KeyCode::Char('?') => self.help = true,
                KeyCode::Char('s') => self.status = true,
                KeyCode::Char('l') => {
                    self.linear.start(true);
                    self.status = true;
                }
                KeyCode::Char('r') => self.linear.start(false),
                KeyCode::Char('j' | 'k') | KeyCode::Down | KeyCode::Up => {
                    self.linear.restore_selection = None;
                    self.linear
                        .select(if matches!(key.code, KeyCode::Char('j') | KeyCode::Down) {
                            1
                        } else {
                            -1
                        });
                    if let Some(issue) = self.linear.issue() {
                        self.workspaces.select(&issue.id)?;
                        self.current = Some(issue.id.clone());
                    }
                }
                KeyCode::Char('c') => self.open_ticket(View::Primary, true, size)?,
                KeyCode::Enter => self.open_ticket(View::Primary, false, size)?,
                KeyCode::Char('e') => self.open_ticket(View::Editor, false, size)?,
                KeyCode::Char('t') => self.open_ticket(View::Shell, false, size)?,
                _ => {}
            }
        }
        Ok(false)
    }

    fn event_loop(&mut self, host: &mut Host, stop: &AtomicBool) -> Result<()> {
        let mut redraw = true;
        loop {
            if host_disconnected()? {
                return Ok(());
            }
            if stop.swap(false, Ordering::Relaxed) {
                if self.request_quit() {
                    return Ok(());
                }
                redraw = true;
            }
            redraw |= self.poll()?;
            if redraw {
                self.draw(host)?;
                redraw = false;
            }
            if !event::poll(Duration::from_millis(16))? {
                continue;
            }
            redraw = true;
            match event::read()? {
                Event::Resize(cols, rows) => {
                    let area = ui::areas(ratatui::layout::Rect::new(0, 0, cols, rows)).content;
                    for session in self.sessions.values() {
                        for child in [&session.primary, &session.editor, &session.shell]
                            .into_iter()
                            .flatten()
                        {
                            child.resize(area.width, area.height)?;
                        }
                    }
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    match self.key(key, terminal::size()?) {
                        Ok(true) => return Ok(()),
                        Ok(false) => {}
                        Err(error) => {
                            self.notice = format!("{error:#}");
                            self.view = View::Manager;
                        }
                    }
                }
                Event::Paste(text) if !self.help && !self.status && !self.quit_prompt => {
                    if let Some(child) = self.session().and_then(|s| s.child(self.view)) {
                        let bracketed = child
                            .parser
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .screen()
                            .bracketed_paste();
                        child.write(&input::paste(&text, bracketed))?;
                    }
                }
                _ => {}
            }
        }
    }
}

fn host_disconnected() -> io::Result<bool> {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: fd refers to one initialized pollfd; polling does not consume input.
    let result = unsafe { libc::poll(&mut fd, 1, 0) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0)
}
