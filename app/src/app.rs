use crate::{
    agent_session,
    config::Config,
    host::{Host, TerminalGuard, host_disconnected, host_io},
    linear_state::Linear,
    session::{Pane, TicketSession, View, content_area},
    settings,
    ticket::PendingPaste,
    workspace::Workspaces,
};
use ::terminal::input;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use crossterm::{
    event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    terminal,
};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use std::{
    collections::HashMap,
    io::{self, IsTerminal, Write},
    process::Child,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

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
    let mut shutdown = None;
    for session in app.sessions.values_mut() {
        if let Err(error) = session.shutdown() {
            shutdown.get_or_insert(error);
        }
    }
    let shutdown = shutdown.map_or(Ok(()), Err);
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
        match self.session() {
            Some(session) => session.pane_view(self.view),
            None if self.view == View::Manager => View::Overview,
            None => self.view,
        }
    }

    /// The area a child terminal occupies at the given host size.
    fn content_area(&self, size: (u16, u16)) -> Rect {
        content_area(size, self.sidebar_visible)
    }

    /// Whether a popup owns the keyboard, so children must not receive input.
    fn modal_open(&self) -> bool {
        self.help || self.status || self.quit_prompt || self.linear.picker.is_some()
    }

    /// The view to show when focus leaves the sidebar: the ticket's own pane
    /// when it has one, and the overview otherwise.
    fn focus_content(&self) -> View {
        self.session()
            .filter(|session| session.has_content())
            .map_or(View::Overview, |session| session.last)
    }

    fn focus_sidebar(&mut self, size: (u16, u16)) -> Result<()> {
        self.view = View::Manager;
        if !self.sidebar_visible {
            self.sidebar_visible = true;
            self.resize_children(size)?;
        }
        Ok(())
    }

    /// Focus a ticket, creating its workspace and session on first use, and
    /// open the terminal the requested view shows.
    fn open_ticket(&mut self, view: View, paste: bool, size: (u16, u16)) -> Result<()> {
        let issue = self
            .linear
            .issue()
            .cloned()
            .context("Select a Linear ticket first")?;
        self.ensure_session(&issue)?;
        self.current = Some(issue.id.clone());
        let area = self.content_area(size);
        match view {
            View::Primary => self.open_agent(&issue, paste, area)?,
            View::Editor | View::Shell => {
                let pane = view.pane().expect("editor and shell are panes");
                self.session_mut(&issue.id).open_pane(pane, area)?;
            }
            View::Manager | View::Overview => {}
        }
        self.session_mut(&issue.id).last = view;
        self.view = view;
        self.notice.clear();
        Ok(())
    }

    fn session_mut(&mut self, id: &str) -> &mut TicketSession {
        self.sessions.get_mut(id).expect("ticket session exists")
    }

    /// Claim the ticket's workspace and hold its lock for as long as the
    /// session lives. Does nothing once the session exists.
    fn ensure_session(&mut self, issue: &linear::Issue) -> Result<()> {
        if self.sessions.contains_key(&issue.id) {
            return Ok(());
        }
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
                children: [None, None, None],
                startup: None,
                starting_at: None,
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
        Ok(())
    }

    /// Open the primary pane. A plain command starts at once; an agent first
    /// prepares its conversation in the background and spawns when that lands.
    fn open_agent(&mut self, issue: &linear::Issue, paste: bool, area: Rect) -> Result<()> {
        // Disjoint borrows: the session is read and written while the global
        // config's one-shot conversation options are cleared.
        let App {
            sessions, config, ..
        } = self;
        let session = sessions.get_mut(&issue.id).expect("ticket session exists");
        if session.pane(Pane::Primary).is_some() {
            // The agent is already running, so Enter sends the ticket again.
            // A prompt that was busy when the ticket first arrived, such as
            // one asking about a new directory, never received it.
            if paste && session.config.agent_mode {
                session.paste = Some(PendingPaste::new(issue));
            }
            return Ok(());
        }
        if session.startup.is_some() {
            return Ok(());
        }
        if !session.config.agent_mode {
            return session.open_pane(Pane::Primary, area);
        }
        let seed = paste && (session.config.resume.is_none() || session.config.new_conversation);
        tracing::info!(ticket_id = %issue.id, agent = session.config.agent.name(), "Starting agent");
        session.startup = Some(agent_session::Startup::start(
            session.config.clone(),
            seed.then(|| issue.clone()),
        ));
        session.starting_at = Some(Instant::now());
        config.resume = None;
        config.new_conversation = false;
        Ok(())
    }

    fn poll(&mut self) -> Result<bool> {
        let mut redraw = self.linear.poll();
        redraw |= self.poll_link_openers();
        redraw |= self.poll_linear()?;
        redraw |= self.poll_sessions()?;
        redraw |= self.poll_cleanup()?;
        Ok(redraw)
    }

    /// Report the outcome of the external URL handler once it exits.
    fn poll_link_openers(&mut self) -> bool {
        let mut redraw = false;
        let notice = &mut self.notice;
        self.link_openers.retain_mut(|child| match child.try_wait() {
            Ok(None) => true,
            result => {
                *notice = match result {
                    Ok(Some(status)) if status.success() => "Opened issue in Linear.".into(),
                    _ => "Could not open Linear. Check your system's default browser or URL handler."
                        .into(),
                };
                redraw = true;
                false
            }
        });
        redraw
    }

    /// Retire workspaces whose tickets closed, and refresh issues on a timer.
    fn poll_linear(&mut self) -> Result<bool> {
        if !self.linear.closed.is_empty() {
            self.workspaces
                .mark_closed(&std::mem::take(&mut self.linear.closed))?;
            self.next_cleanup = Instant::now();
        }
        if Instant::now() >= self.next_refresh {
            self.linear.start(false);
            self.next_refresh = Instant::now() + Duration::from_secs(60);
        }
        Ok(false)
    }

    /// Advance every ticket: finish agent startup, reap exited terminals, and
    /// deliver a pending ticket paste once the agent accepts one.
    fn poll_sessions(&mut self) -> Result<bool> {
        // Destructuring keeps the field borrows disjoint while the loop holds
        // `sessions` mutably and still writes to `view` and `notice`.
        let App {
            sessions,
            workspaces,
            current,
            view: current_view,
            sidebar_visible,
            notice,
            ..
        } = self;
        let mut redraw = false;
        for (id, session) in sessions.iter_mut() {
            let visible = current.as_ref() == Some(id);
            let pane_view = session.pane_view(*current_view);
            match session.advance_startup(id, workspaces, *sidebar_visible) {
                Ok(false) => {}
                Ok(true) => redraw = true,
                Err(error) => {
                    crate::logging::failure("agent_startup", &error);
                    *notice = format!("Agent startup failed: {error:#}");
                    if visible {
                        *current_view = View::Manager;
                    }
                    redraw = true;
                }
            }
            redraw |= session.note_first_output(id);
            for (pane, slot) in Pane::ALL.into_iter().zip(&mut session.children) {
                let Some(child) = slot else { continue };
                if child.poll_exit()? {
                    tracing::info!(ticket_id = %id, "Ticket terminal exited");
                    child.shutdown()?;
                    *slot = None;
                    if pane == Pane::Primary {
                        session.starting_at = None;
                    }
                    if visible && pane_view == pane.view() {
                        *current_view = View::Manager;
                        *notice = "Terminal exited. Open it again to restart.".into();
                        redraw = true;
                    }
                } else {
                    redraw |= child.changed.swap(false, Ordering::Relaxed)
                        && visible
                        && pane_view == pane.view();
                }
            }
            if let Err(error) = session.deliver_paste(visible && *current_view == View::Primary) {
                crate::logging::failure("ticket_paste", &error);
                *notice = error.to_string();
                *current_view = View::Manager;
                redraw = true;
            }
        }
        Ok(redraw)
    }

    /// Remove the workspaces of closed tickets once nothing is using them.
    fn poll_cleanup(&mut self) -> Result<bool> {
        if Instant::now() < self.next_cleanup {
            return Ok(false);
        }
        let mut redraw = false;
        for record in self.workspaces.list()?.into_iter().filter(|w| w.cleanup) {
            if self
                .sessions
                .get(&record.ticket_id)
                .is_some_and(TicketSession::has_children)
            {
                self.notice = "Closed ticket workspace retained until its terminals exit.".into();
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
                        "Closed ticket workspace retained because it has local changes.".into();
                }
                Err(error) => {
                    crate::logging::failure("workspace_cleanup", &error);
                    self.notice = format!("Workspace cleanup deferred: {error:#}");
                }
            }
            redraw = true;
        }
        self.next_cleanup = Instant::now() + Duration::from_secs(5);
        Ok(redraw)
    }

    fn draw(&self, host: &mut Host) -> io::Result<()> {
        host.draw(|frame| {
            let tickets = self.tickets();
            let display = ui::Tickets {
                items: &tickets,
                selected: self.linear.selected,
                connected: self.linear.snapshot.is_some(),
                busy: self.linear.busy_elapsed(),
            };
            let content = ui::draw(
                frame,
                &display,
                &self.workspaces.source.to_string_lossy(),
                self.view == View::Manager,
                self.sidebar_visible,
            );
            self.draw_content(frame, content, &display);
            self.draw_overlays(frame);
        })?;
        Ok(())
    }

    /// Borrow the cached issues as the view model the `ui` crate draws from.
    fn tickets(&self) -> Vec<ui::Ticket<'_>> {
        self.linear.snapshot.as_ref().map_or_else(Vec::new, |s| {
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
    }

    /// The main pane: a live child terminal, a frozen selection over it, or the
    /// ticket overview while nothing is running there.
    fn draw_content(&self, frame: &mut ratatui::Frame, content: Rect, display: &ui::Tickets<'_>) {
        let starting = self.session().and_then(|session| {
            session
                .starting_at
                .map(|started| (session.config.agent.name(), started.elapsed()))
        });
        let pane_view = self.pane_view();
        // An agent that has not painted yet still shows its startup banner.
        let child = self
            .session()
            .and_then(|s| s.child(pane_view))
            .filter(|_| !(pane_view == View::Primary && starting.is_some()));
        let Some(child) = child else {
            ui::draw_details(
                frame,
                content,
                ui::details(display, &self.notice),
                self.view == View::Overview,
                starting,
            );
            return;
        };
        if let Some(selection) = &self.selection {
            frame.render_widget(selection, content);
        } else {
            let parser = child.parser.lock().unwrap_or_else(|e| e.into_inner());
            ui::draw_terminal(
                frame,
                content,
                parser.screen(),
                self.view != View::Manager && !self.modal_open(),
            );
        }
    }

    fn draw_overlays(&self, frame: &mut ratatui::Frame) {
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
                    busy: self.linear.busy(),
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
            ui::status_popup(frame, self.status_text());
        }
        if self.quit_prompt {
            ui::quit_popup(frame);
        }
    }

    fn status_text(&self) -> String {
        let directory = self
            .session()
            .map_or(self.workspaces.source.as_path(), |s| s.config.cwd.as_path());
        let logs = crate::logging::directory()
            .map_or_else(|_| "Unavailable".into(), |path| path.display().to_string());
        format!(
            "{}\n\nLinear workspace: {}\nRepository: {}\n\nDirectory: {}\nLogs: {}\n\n{}",
            self.linear.status,
            self.linear
                .workspace
                .as_ref()
                .map_or("Not mapped — press l to connect", |w| w.name.as_str()),
            self.workspaces.source.display(),
            directory.display(),
            logs,
            self.notice
        )
    }

    fn request_quit(&mut self) -> bool {
        if self.sessions.values().any(TicketSession::protected) {
            self.quit_prompt = true;
            false
        } else {
            true
        }
    }

    /// Route a key to whatever currently owns the keyboard. Returns `true` to quit.
    fn key(&mut self, key: KeyEvent, size: (u16, u16)) -> Result<bool> {
        self.selection = None;
        if self.quit_prompt {
            return Ok(self.key_quit_prompt(key));
        }
        if self.linear.picker.is_some() {
            self.key_picker(key);
        } else if self.status {
            self.status = !matches!(key.code, KeyCode::Esc | KeyCode::Char('s'));
        } else if self.help {
            self.help = !matches!(key.code, KeyCode::Esc | KeyCode::Char('?'));
        } else if let Some(code) = self.pane_navigation_key(key) {
            match code {
                'h' => self.focus_sidebar(size)?,
                _ => self.view = self.focus_content(),
            }
        } else if self.config.is_escape(key) {
            self.focus_sidebar(size)?;
        } else if !matches!(self.view, View::Manager | View::Overview) {
            self.key_child(key)?;
        } else {
            return self.key_manager(key, size);
        }
        Ok(false)
    }

    fn key_quit_prompt(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('f' | 'F') => return true,
            KeyCode::Esc => self.quit_prompt = false,
            _ => {}
        }
        false
    }

    fn key_picker(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('l') => self.linear.close_picker(),
            KeyCode::Enter => {
                self.linear.confirm_workspace();
                // Saving continues in the background; show where it is reported.
                if self.linear.picker.is_none() && self.linear.busy() {
                    self.status = true;
                }
            }
            KeyCode::Char('r') if !self.linear.busy() => self.linear.open_picker(),
            _ => {}
        }
    }

    /// `Ctrl-H`/`Ctrl-L` move focus between the sidebar and the content pane,
    /// but only while the sidebar has the keyboard; a child owns them otherwise.
    fn pane_navigation_key(&self, key: KeyEvent) -> Option<char> {
        let KeyCode::Char(code @ ('h' | 'l')) = key.code else {
            return None;
        };
        (self.config.pane_navigation
            && matches!(self.view, View::Manager | View::Overview)
            && key.modifiers == KeyModifiers::CONTROL)
            .then_some(code)
    }

    /// Forward a key to the focused child, returning it to the live screen first.
    fn key_child(&mut self, key: KeyEvent) -> Result<()> {
        let Some(child) = self.session().and_then(|s| s.child(self.view)) else {
            return Ok(());
        };
        let cursor = {
            let mut parser = child.parser.lock().unwrap_or_else(|e| e.into_inner());
            parser.screen_mut().set_scrollback(0);
            parser.screen().application_cursor()
        };
        child.write(&input::encode(key, cursor))
    }

    /// Ticket and application shortcuts, active while the sidebar has focus.
    fn key_manager(&mut self, key: KeyEvent, size: (u16, u16)) -> Result<bool> {
        match key.code {
            KeyCode::Char('q') => return Ok(self.request_quit()),
            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                return Ok(self.request_quit());
            }
            KeyCode::Char('f') => {
                self.sidebar_visible = !self.sidebar_visible;
                self.resize_children(size)?;
                if !self.sidebar_visible {
                    self.view = self.focus_content();
                }
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('s') => self.status = true,
            KeyCode::Char('l') => self.linear.open_picker(),
            KeyCode::Char('r') => self.linear.start(false),
            KeyCode::Char('o') => self.open_in_linear()?,
            KeyCode::Char('j' | 'k') | KeyCode::Down | KeyCode::Up => {
                let step = if matches!(key.code, KeyCode::Char('j') | KeyCode::Down) {
                    1
                } else {
                    -1
                };
                self.linear.select(step);
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
        Ok(false)
    }

    /// Hand the selected ticket to the system browser, reported once it exits.
    fn open_in_linear(&mut self) -> Result<()> {
        let issue = self
            .linear
            .issue()
            .context("Select a Linear ticket first")?;
        anyhow::ensure!(
            issue.url.starts_with("https://linear.app/"),
            "This ticket does not have a valid Linear URL"
        );
        let child = crate::browser::open(&issue.url).spawn().with_context(|| {
            format!(
                "Cannot open Linear; check that {} is installed",
                crate::browser::program()
            )
        })?;
        self.link_openers.push(child);
        self.notice = format!("Opening {} in Linear…", issue.identifier);
        Ok(())
    }

    fn mouse(&mut self, mut mouse: MouseEvent, size: (u16, u16)) -> Result<MouseOutcome> {
        let area = self.content_area(size);
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
        let area = self.content_area(size);
        for session in self.sessions.values() {
            for child in session.children.iter().flatten() {
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
            if (self.linear.busy() || self.session().is_some_and(|s| s.starting_at.is_some()))
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
                Event::Mouse(mouse) if !self.modal_open() => {
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
                Event::Paste(text) if !self.modal_open() => {
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
