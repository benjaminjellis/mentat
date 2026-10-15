use crate::{
    agent_session, config::Config, linear_state::Linear, settings, ticket::PendingPaste,
    workspace::Workspaces,
};
use ::terminal::{input, pty::Session};
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use crossterm::{
    cursor::Show,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, IsTerminal, Write},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

type Host = Terminal<CrosstermBackend<Box<dyn Write>>>;

/// Wheel step through a child's history, in lines.
const SCROLL_LINES: usize = 3;

/// A busy child can change far faster than anyone can read. Draw at most this
/// often, and wait at most this long for the next host event.
const FRAME: Duration = Duration::from_millis(33);
const POLL: Duration = Duration::from_millis(16);

/// What a mouse event asks of us: text for the host clipboard, and whether our
/// own view changed. Events forwarded to a child redraw when the child answers.
#[derive(Default)]
struct MouseOutcome {
    copied: Option<String>,
    redraw: bool,
}

impl MouseOutcome {
    fn redraw() -> Self {
        Self {
            copied: None,
            redraw: true,
        }
    }
}

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self> {
        let guard = Self;
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            DisableMouseCapture,
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
    Overview,
    Primary,
    Editor,
    Shell,
}

struct TicketSession {
    config: Config,
    primary: Option<Session>,
    startup: Option<agent_session::Startup>,
    starting_at: Option<Instant>,
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
            View::Manager | View::Overview => None,
        }
    }
    fn has_children(&self) -> bool {
        self.startup.is_some()
            || self.primary.is_some()
            || self.editor.is_some()
            || self.shell.is_some()
    }
    fn protected(&self) -> bool {
        self.editor.is_some()
            || self.shell.is_some()
            || (self.config.protect_child && self.primary.is_some())
    }
    fn shutdown(&mut self) -> Result<()> {
        self.startup = None;
        self.starting_at = None;
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
    sidebar_visible: bool,
    selection: Option<ui::TerminalSelection>,
    link_openers: Vec<Child>,
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
    tracing::info!(workspace_backend = ?config.workspace, agent = config.agent.name(), "Opening repository");
    let workspaces = Workspaces::open(
        &config.cwd,
        &settings::state_directory()?,
        config.workspace_root.as_deref(),
        config.workspace,
    )?;
    let tracked = workspaces
        .list()?
        .into_iter()
        .map(|w| w.ticket_id)
        .collect();
    let (store, snapshot, workspace) =
        workspaces.linear_state(&settings::directory()?, config.linear.as_ref())?;
    let linear = Linear::new(config.linear.clone(), tracked, store, snapshot, workspace);
    let mut app = App {
        config,
        workspaces,
        sessions: HashMap::new(),
        current: None,
        linear,
        view: View::Manager,
        sidebar_visible: true,
        selection: None,
        link_openers: Vec::new(),
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
    let output: Box<dyn Write> = Box::new(io::stdout());
    let mut host = Terminal::new(CrosstermBackend::new(output))?;
    let result = app.event_loop(&mut host, &stop);
    // TerminalGuard restores the real terminal. Ratatui's Drop also tries to
    // show the cursor and uses eprintln! on failure, which panics if the host
    // has closed stderr too. Discard that redundant cleanup output.
    *host.backend_mut() = CrosstermBackend::new(Box::new(io::sink()));
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

    fn pane_view(&self) -> View {
        if self.view == View::Manager {
            self.session()
                .map_or(View::Overview, |session| session.last)
        } else {
            self.view
        }
    }

    fn focus_sidebar(&mut self, size: (u16, u16)) -> Result<()> {
        self.view = View::Manager;
        if !self.sidebar_visible {
            self.sidebar_visible = true;
            self.resize_children(size)?;
        }
        Ok(())
    }

    fn open_ticket(&mut self, view: View, paste: bool, size: (u16, u16)) -> Result<()> {
        let issue = self
            .linear
            .issue()
            .cloned()
            .context("Select a Linear ticket first")?;
        if !self.sessions.contains_key(&issue.id) {
            let record = self.workspaces.ensure(&issue.id, &issue.identifier)?;
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
                    startup: None,
                    starting_at: None,
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
        self.current = Some(issue.id.clone());
        let session = self
            .sessions
            .get_mut(&issue.id)
            .expect("ticket session exists");
        let area = ui::areas(
            ratatui::layout::Rect::new(0, 0, size.0, size.1),
            self.sidebar_visible,
        )
        .content;
        match view {
            View::Primary => {
                if session.primary.is_none()
                    && session.startup.is_none()
                    && !session.config.agent_mode
                {
                    session.primary = Some(Session::spawn(
                        &session.config.command,
                        &session.config.cwd,
                        area.width,
                        area.height,
                    )?);
                } else if session.primary.is_none() && session.startup.is_none() {
                    let paste_ticket = paste
                        && session.config.agent_mode
                        && (session.config.resume.is_none() || session.config.new_conversation);
                    tracing::info!(ticket_id = %issue.id, agent = session.config.agent.name(), "Starting agent");
                    session.startup = Some(agent_session::Startup::start(
                        session.config.clone(),
                        paste_ticket.then_some(issue.clone()),
                    ));
                    session.starting_at = Some(Instant::now());
                    self.config.resume = None;
                    self.config.new_conversation = false;
                } else if paste && session.config.agent_mode && session.primary.is_some() {
                    // The agent is already running, so Enter sends the ticket again.
                    // A prompt that was busy when the ticket first arrived, such as
                    // one asking about a new directory, never received it.
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
            View::Manager | View::Overview => {}
        }
        session.last = view;
        self.view = view;
        self.notice.clear();
        Ok(())
    }

    fn poll(&mut self) -> Result<bool> {
        let mut redraw = self.linear.poll();
        self.link_openers.retain_mut(|child| match child.try_wait() {
            Ok(None) => true,
            result => {
                self.notice = match result {
                    Ok(Some(status)) if status.success() => "Opened issue in Linear.".into(),
                    _ => "Could not open Linear. Check your system's default browser or URL handler.".into(),
                };
                redraw = true;
                false
            }
        });
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
            let pane_view = if self.view == View::Manager {
                session.last
            } else {
                self.view
            };
            if let Some(startup) = &session.startup {
                let result = startup.poll().and_then(|config| {
                    let Some(config) = config else {
                        return Ok(false);
                    };
                    session.config = config;
                    if let Some(conversation) = &session.config.resume {
                        self.workspaces.save_conversation(
                            id,
                            session.config.agent.name(),
                            conversation,
                        )?;
                    }
                    let size = terminal::size()?;
                    let area = ui::areas(
                        ratatui::layout::Rect::new(0, 0, size.0, size.1),
                        self.sidebar_visible,
                    )
                    .content;
                    session.primary = Some(Session::spawn(
                        &session.config.command,
                        &session.config.cwd,
                        area.width,
                        area.height,
                    )?);
                    session.config.new_conversation = false;
                    session.paste = startup.paste.as_ref().map(PendingPaste::new);
                    Ok(true)
                });
                match result {
                    Ok(false) => {}
                    Ok(true) => {
                        session.startup = None;
                        redraw = true;
                    }
                    Err(error) => {
                        crate::logging::failure("agent_startup", &error);
                        session.startup = None;
                        session.starting_at = None;
                        self.notice = format!("Agent startup failed: {error:#}");
                        if visible {
                            self.view = View::Manager;
                        }
                        redraw = true;
                    }
                }
            }
            if session.starting_at.is_some()
                && let Some(primary) = &session.primary
            {
                let parser = primary.parser.lock().unwrap_or_else(|e| e.into_inner());
                if !parser.screen().contents().trim().is_empty() {
                    tracing::info!(ticket_id = %id, elapsed_ms = session.starting_at.unwrap().elapsed().as_millis() as u64, "Agent produced output");
                    session.starting_at = None;
                    redraw = true;
                }
            }
            for (view, slot) in [
                (View::Primary, &mut session.primary),
                (View::Editor, &mut session.editor),
                (View::Shell, &mut session.shell),
            ] {
                if let Some(child) = slot {
                    if child.poll_exit()? {
                        tracing::info!(ticket_id = %id, "Ticket terminal exited");
                        child.shutdown()?;
                        *slot = None;
                        if view == View::Primary {
                            session.starting_at = None;
                        }
                        if visible && pane_view == view {
                            self.view = View::Manager;
                            self.notice = "Terminal exited. Open it again to restart.".into();
                            redraw = true;
                        }
                    } else {
                        redraw |= child.changed.swap(false, Ordering::Relaxed)
                            && visible
                            && pane_view == view;
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
                        crate::logging::failure("ticket_paste", &error);
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
                    Err(error) => {
                        crate::logging::failure("workspace_cleanup", &error);
                        self.notice = format!("Workspace cleanup deferred: {error:#}");
                    }
                }
                redraw = true;
            }
            self.next_cleanup = Instant::now() + Duration::from_secs(5);
        }
        Ok(redraw)
    }

    fn draw(&self, host: &mut Host) -> io::Result<()> {
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
            let content = ui::draw(
                frame,
                &display,
                &self.workspaces.source.to_string_lossy(),
                self.view == View::Manager,
                self.sidebar_visible,
            );
            let starting = self.session().and_then(|session| {
                session
                    .starting_at
                    .map(|started| (session.config.agent.name(), started.elapsed()))
            });
            let pane_view = self.pane_view();
            if let Some(child) = self.session().and_then(|s| s.child(pane_view))
                && !(pane_view == View::Primary && starting.is_some())
            {
                let parser = child.parser.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(selection) = &self.selection {
                    frame.render_widget(selection, content);
                } else {
                    ui::draw_terminal(
                        frame,
                        content,
                        parser.screen(),
                        self.view != View::Manager
                            && !self.help
                            && !self.status
                            && !self.quit_prompt
                            && self.linear.picker.is_none(),
                    );
                }
            } else {
                ui::draw_details(
                    frame,
                    content,
                    ui::details(&display, &self.notice),
                    self.view == View::Overview,
                    starting,
                );
            }
            if let Some(picker) = &self.linear.picker {
                ui::workspace_picker(
                    frame,
                    ui::WorkspacePicker {
                        repository: &self.workspaces.source.to_string_lossy(),
                        current: self.linear.workspace.as_ref().map(|w| w.name.as_str()),
                        available: picker.available.as_ref().map(|w| w.name.as_str()),
                        is_current: picker
                            .available
                            .as_ref()
                            .zip(self.linear.workspace.as_ref())
                            .is_some_and(|(a, b)| a.id == b.id),
                        api_key: picker.api_key,
                        busy: self.linear.busy,
                        message: &picker.message,
                    },
                );
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
                        "{}\n\nLinear workspace: {}\nRepository: {}\n\nDirectory: {}\nLogs: {}\n\n{}",
                        self.linear.status,
                        self.linear
                            .workspace
                            .as_ref()
                            .map_or("Not mapped — press l to connect", |w| w.name.as_str()),
                        self.workspaces.source.display(),
                        path.display(),
                        crate::logging::directory().map(|path| path.display().to_string()).unwrap_or_else(|_| "Unavailable".into()),
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
        self.selection = None;
        if self.quit_prompt {
            match key.code {
                KeyCode::Char('f' | 'F') => return Ok(true),
                KeyCode::Esc => self.quit_prompt = false,
                _ => {}
            }
        } else if self.linear.picker.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('l') => self.linear.close_picker(),
                KeyCode::Enter => {
                    self.linear.confirm_workspace();
                    if self.linear.picker.is_none() && self.linear.busy {
                        self.status = true;
                    }
                }
                KeyCode::Char('r') if !self.linear.busy => self.linear.open_picker(),
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
            && matches!(self.view, View::Manager | View::Overview)
            && key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('h' | 'l'))
        {
            match key.code {
                KeyCode::Char('h') => self.focus_sidebar(size)?,
                KeyCode::Char('l') => {
                    self.view = self
                        .session()
                        .filter(|session| {
                            session.child(session.last).is_some() || session.startup.is_some()
                        })
                        .map_or(View::Overview, |session| session.last);
                }
                _ => {}
            }
        } else if self.config.is_escape(key) {
            self.focus_sidebar(size)?;
        } else if !matches!(self.view, View::Manager | View::Overview) {
            if let Some(child) = self.session().and_then(|s| s.child(self.view)) {
                let cursor = {
                    let mut parser = child.parser.lock().unwrap_or_else(|e| e.into_inner());
                    parser.screen_mut().set_scrollback(0);
                    parser.screen().application_cursor()
                };
                child.write(&input::encode(key, cursor))?;
            }
        } else if key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL)
        {
            return Ok(self.request_quit());
        } else {
            match key.code {
                KeyCode::Char('f') => {
                    self.sidebar_visible = !self.sidebar_visible;
                    self.resize_children(size)?;
                    if !self.sidebar_visible {
                        self.view = self
                            .session()
                            .filter(|session| {
                                session.child(session.last).is_some() || session.startup.is_some()
                            })
                            .map_or(View::Overview, |session| session.last);
                    }
                }
                KeyCode::Char('?') => self.help = true,
                KeyCode::Char('s') => self.status = true,
                KeyCode::Char('l') => {
                    self.linear.open_picker();
                }
                KeyCode::Char('r') => self.linear.start(false),
                KeyCode::Char('o') => {
                    let issue = self
                        .linear
                        .issue()
                        .context("Select a Linear ticket first")?;
                    anyhow::ensure!(
                        issue.url.starts_with("https://linear.app/"),
                        "This ticket does not have a valid Linear URL"
                    );
                    let opener = if cfg!(target_os = "macos") {
                        "open"
                    } else {
                        "xdg-open"
                    };
                    let child = Command::new(opener)
                        .arg(&issue.url)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .with_context(|| {
                            format!("Cannot open Linear; check that {opener} is installed")
                        })?;
                    self.link_openers.push(child);
                    self.notice = format!("Opening {} in Linear…", issue.identifier);
                }
                KeyCode::Char('j' | 'k') | KeyCode::Down | KeyCode::Up => {
                    self.linear
                        .select(if matches!(key.code, KeyCode::Char('j') | KeyCode::Down) {
                            1
                        } else {
                            -1
                        });
                    if let Some(issue) = self.linear.issue() {
                        self.current = Some(issue.id.clone());
                    }
                    self.notice.clear();
                }
                KeyCode::Char('a') => self.open_ticket(View::Primary, false, size)?,
                KeyCode::Enter => self.open_ticket(View::Primary, true, size)?,
                KeyCode::Char('e') => self.open_ticket(View::Editor, false, size)?,
                KeyCode::Char('t') => self.open_ticket(View::Shell, false, size)?,
                _ => {}
            }
        }
        Ok(false)
    }

    fn mouse(&mut self, mut mouse: MouseEvent, size: (u16, u16)) -> Result<MouseOutcome> {
        let area = ui::areas(
            ratatui::layout::Rect::new(0, 0, size.0, size.1),
            self.sidebar_visible,
        )
        .content;
        let inside = area.contains((mouse.column, mouse.row).into());
        mouse.column = mouse
            .column
            .saturating_sub(area.x)
            .min(area.width.saturating_sub(1));
        mouse.row = mouse
            .row
            .saturating_sub(area.y)
            .min(area.height.saturating_sub(1));

        if let Some(selection) = &mut self.selection
            && selection.dragging
            && matches!(
                mouse.kind,
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            )
        {
            selection.update(mouse.row, mouse.column);
            if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                selection.dragging = false;
                if selection.moved() {
                    return Ok(MouseOutcome {
                        copied: Some(selection.text()),
                        redraw: true,
                    });
                }
                self.selection = None;
            }
            return Ok(MouseOutcome::redraw());
        }

        let mut outcome = MouseOutcome::default();
        if matches!(
            mouse.kind,
            MouseEventKind::Down(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) {
            outcome.redraw |= self.selection.take().is_some();
        }
        if inside && let Some(child) = self.session().and_then(|s| s.child(self.pane_view())) {
            let mut parser = child.parser.lock().unwrap_or_else(|e| e.into_inner());
            let screen = parser.screen();
            let mode = screen.mouse_protocol_mode();
            // Agents that never ask for mouse reporting leave the wheel and the
            // selection to us; the others steer their own view.
            let ours = mode == ::terminal::input::MouseProtocolMode::None;
            if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && (self.view == View::Manager || ours)
            {
                let selection = ui::TerminalSelection::new(screen.clone(), mouse.row, mouse.column);
                drop(parser);
                self.selection = Some(selection);
            } else if ours
                && matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                )
            {
                // The emulator keeps the history the child scrolled away; show it.
                let screen = parser.screen_mut();
                let offset = if mouse.kind == MouseEventKind::ScrollUp {
                    screen.scrollback().saturating_add(SCROLL_LINES)
                } else {
                    screen.scrollback().saturating_sub(SCROLL_LINES)
                };
                let before = screen.scrollback();
                screen.set_scrollback(offset);
                outcome.redraw |= screen.scrollback() != before;
            } else if self.view != View::Manager {
                let bytes = input::mouse(mouse, mode, screen.mouse_protocol_encoding());
                drop(parser);
                child.write(&bytes)?;
            }
        }
        Ok(outcome)
    }

    fn resize_children(&self, size: (u16, u16)) -> Result<()> {
        let area = ui::areas(
            ratatui::layout::Rect::new(0, 0, size.0, size.1),
            self.sidebar_visible,
        )
        .content;
        for session in self.sessions.values() {
            for child in [&session.primary, &session.editor, &session.shell]
                .into_iter()
                .flatten()
            {
                child.resize(area.width, area.height)?;
            }
        }
        Ok(())
    }

    fn event_loop(&mut self, host: &mut Host, stop: &AtomicBool) -> Result<()> {
        let mut redraw = true;
        let mut last_draw = Instant::now() - FRAME;
        let mut last_animation = Instant::now();
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
            if (self.linear.busy || self.session().is_some_and(|s| s.starting_at.is_some()))
                && last_animation.elapsed() >= Duration::from_millis(100)
            {
                redraw = true;
                last_animation = Instant::now();
            }
            if redraw && last_draw.elapsed() >= FRAME {
                if host_io(self.draw(host))?.is_none() {
                    return Ok(());
                }
                redraw = false;
                last_draw = Instant::now();
            }
            // A pending frame must not wait out a full poll interval.
            let wait = if redraw {
                FRAME.saturating_sub(last_draw.elapsed()).min(POLL)
            } else {
                POLL
            };
            let Some(ready) = host_io(event::poll(wait))? else {
                return Ok(());
            };
            if !ready {
                continue;
            }
            let Some(event) = host_io(event::read())? else {
                return Ok(());
            };
            match event {
                Event::Resize(cols, rows) => {
                    self.selection = None;
                    self.resize_children((cols, rows))?;
                    redraw = true;
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let Some(size) = host_io(terminal::size())? else {
                        return Ok(());
                    };
                    redraw = true;
                    match self.key(key, size) {
                        Ok(true) => return Ok(()),
                        Ok(false) => {}
                        Err(error) => {
                            crate::logging::failure("ui_action", &error);
                            self.notice = format!("{error:#}");
                            self.view = View::Manager;
                        }
                    }
                }
                Event::Mouse(mouse)
                    if !self.help
                        && !self.status
                        && !self.quit_prompt
                        && self.linear.picker.is_none() =>
                {
                    let Some((cols, rows)) = host_io(terminal::size())? else {
                        return Ok(());
                    };
                    let outcome = self.mouse(mouse, (cols, rows))?;
                    redraw |= outcome.redraw;
                    if let Some(text) = outcome.copied {
                        // OSC 52 writes the selection to the host clipboard, including over SSH.
                        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
                        write!(host.backend_mut(), "\x1b]52;c;{encoded}\x1b\\")?;
                        host.backend_mut().flush()?;
                    }
                }
                Event::Paste(text)
                    if !self.help
                        && !self.status
                        && !self.quit_prompt
                        && self.linear.picker.is_none() =>
                {
                    redraw = true;
                    self.selection = None;
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

fn host_io<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        // The host can close after the event loop's initial poll. Reads, writes,
        // and size queries may then fail before the next disconnection check.
        Err(_) if host_disconnected().unwrap_or(false) => Ok(None),
        Err(error) => Err(error),
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
