use crate::{
    agent_session,
    config::Config,
    host::{Host, TerminalGuard, draw_frame, host_disconnected, host_io},
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
    cursor::SetCursorStyle,
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
    io::{self, BufWriter, IsTerminal, Write},
    process::Child,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// A busy child can change far faster than anyone can read. Draw at most this
/// often, and wait at most this long for the next host event.
const FRAME: Duration = Duration::from_millis(16);
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
    link_openers: Vec<(String, Child)>,
    pushes: HashMap<String, crate::workspace::Push>,
    help: bool,
    status: bool,
    quit_prompt: bool,
    notice: String,
    forms: HashMap<String, crate::issue_form::Form>,
    editing: Option<String>,
    activity: HashMap<String, Vec<String>>,
    activity_scroll: usize,
    activity_popup: bool,
    activity_popup_scroll: usize,
    overview_scroll: u16,
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
        pushes: HashMap::new(),
        help: false,
        status: false,
        quit_prompt: false,
        notice: String::new(),
        forms: HashMap::new(),
        editing: None,
        activity: HashMap::new(),
        activity_scroll: 0,
        activity_popup: false,
        activity_popup_scroll: 0,
        overview_scroll: 0,
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
    let output: Box<dyn Write> = Box::new(BufWriter::new(io::stdout()));
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
        self.help
            || self.status
            || self.activity_popup
            || self.quit_prompt
            || self.linear.picker.is_some()
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
        let label = match view {
            View::Primary => "Agent opened",
            View::Editor => "Editor opened",
            View::Shell => "Shell opened",
            _ => "Ticket opened",
        };
        if let Some(id) = self.current.clone() {
            self.record(id, label.into());
        }
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

    fn record(&mut self, id: String, message: String) {
        record_event(&mut self.activity, id, message);
    }

    fn poll(&mut self) -> Result<bool> {
        let mut redraw = self.linear.poll();
        redraw |= self.poll_edits();
        redraw |= self.poll_link_openers();
        redraw |= self.poll_pushes();
        redraw |= self.poll_linear()?;
        redraw |= self.poll_sessions()?;
        redraw |= self.poll_cleanup()?;
        Ok(redraw)
    }

    fn poll_pushes(&mut self) -> bool {
        let mut redraw = false;
        self.pushes.retain(|id, push| {
            let message = match push.poll() {
                Ok(None) => return true,
                Ok(Some(message)) => format!("DONE\n{message}"),
                Err(error) => format!("FAILED\n{error:#}"),
            };
            self.notice = message.clone();
            record_event(&mut self.activity, id.clone(), message);
            redraw = true;
            false
        });
        redraw
    }

    fn poll_edits(&mut self) -> bool {
        let mut redraw = false;
        for (id, result) in std::mem::take(&mut self.linear.edited) {
            match result {
                Ok(result) => {
                    if result.submitted {
                        self.record(id.clone(), "DONE\nSaved to Linear.".into());
                    }
                    let form = self.forms.entry(id).or_default();
                    if let Some(details) = result.details {
                        if let Some(snapshot) = &mut self.linear.snapshot
                            && let Some(issue) = snapshot
                                .issues
                                .iter_mut()
                                .find(|i| i.id == details.issue.id)
                        {
                            *issue = details.issue.clone();
                        }
                        form.loaded(details, result.submitted);
                    } else {
                        form.busy = false;
                        if form.posting {
                            form.comment.clear();
                        } else {
                            form.dirty = false;
                            form.details = None;
                        }
                    }
                    if let Some(warning) = result.warning {
                        form.message = warning;
                    }

                    if result.submitted {
                        self.linear.start(false);
                    }
                }
                Err(error) => {
                    self.record(id.clone(), format!("FAILED\n{error}"));
                    let form = self.forms.entry(id).or_default();
                    form.busy = false;
                    form.message = error;
                }
            }
            redraw = true;
        }
        if let Some(id) = self.linear.issue().map(|issue| issue.id.clone())
            && !self.forms.contains_key(&id)
        {
            let mut form = crate::issue_form::Form::default();
            form.load_pending = self.config.linear.is_some();
            if !form.load_pending {
                form.message = "Connect Linear to load comments.".into();
            }
            self.forms.insert(id, form);
            redraw = true;
        }
        if !self.linear.busy()
            && self.linear.picker.is_none()
            && let Some(id) = self
                .forms
                .iter()
                .find(|(_, form)| form.load_pending)
                .map(|(id, _)| id.clone())
        {
            let started = self.linear.edit(id.clone(), ::linear::EditOperation::Load);
            let form = self.forms.get_mut(&id).unwrap();
            form.load_pending = false;
            form.busy = started;
            if !started {
                form.message = "Connect Linear before editing this issue.".into();
            }
            redraw = true;
        }
        redraw
    }

    /// Report the outcome of the external URL handler once it exits.
    fn poll_link_openers(&mut self) -> bool {
        let mut redraw = false;
        self.link_openers.retain_mut(|(id, child)| match child.try_wait() {
            Ok(None) => true,
            result => {
                let message = match result {
                    Ok(Some(status)) if status.success() => "DONE\nOpened issue in Linear.".into(),
                    _ => "FAILED\nCould not open Linear.\nCheck your system's default browser or URL handler."
                        .into(),
                };
                record_event(&mut self.activity, id.clone(), message);
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
            activity,
            ..
        } = self;
        let mut redraw = false;
        for (id, session) in sessions.iter_mut() {
            let visible = current.as_ref() == Some(id);
            let pane_view = session.pane_view(*current_view);
            match session.advance_startup(id, workspaces, *sidebar_visible) {
                Ok(false) => {}
                Ok(true) => {
                    record_event(activity, id.clone(), "Agent started.".into());
                    redraw = true;
                }
                Err(error) => {
                    crate::logging::failure("agent_startup", &error);
                    *notice = format!("FAILED\nAgent startup failed:\n{error:#}");
                    record_event(activity, id.clone(), notice.clone());
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
                    let message = match pane {
                        Pane::Primary => "Agent exited · a to reopen",
                        Pane::Editor => "Editor exited · e to reopen",
                        Pane::Shell => "Shell exited · t to reopen",
                    };
                    record_event(activity, id.clone(), message.into());
                    redraw = true;
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
                *notice = format!("FAILED\n{error}");
                record_event(activity, id.clone(), notice.clone());
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
            if self.pushes.contains_key(&record.ticket_id) {
                continue;
            }
            if self
                .sessions
                .get(&record.ticket_id)
                .is_some_and(TicketSession::has_children)
            {
                self.notice = "Closed ticket workspace retained until its terminals exit.".into();
                self.record(record.ticket_id.clone(), self.notice.clone());
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
                    self.notice = format!("FAILED\nWorkspace cleanup deferred:\n{error:#}");
                }
            }
            self.record(record.ticket_id.clone(), self.notice.clone());
            redraw = true;
        }
        self.next_cleanup = Instant::now() + Duration::from_secs(5);
        Ok(redraw)
    }

    fn draw(&self, host: &mut Host) -> io::Result<()> {
        draw_frame(host, |frame| {
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
            let cursor_style = self.draw_content(frame, content, &display);
            let activity_area = ui::areas(frame.area(), self.sidebar_visible).activity;
            let events = self
                .editing
                .as_deref()
                .or_else(|| self.linear.issue().map(|issue| issue.id.as_str()))
                .and_then(|id| self.activity.get(id));
            ui::draw_activity(
                frame,
                activity_area,
                events.map_or(&[], Vec::as_slice),
                self.activity_scroll,
            );
            self.draw_overlays(frame);
            cursor_style
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
                    has_workspace: self.linear.tracked.contains(&i.id),
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
    fn draw_content(
        &self,
        frame: &mut ratatui::Frame,
        content: Rect,
        display: &ui::Tickets<'_>,
    ) -> SetCursorStyle {
        let starting = self.session().and_then(|session| {
            session
                .starting_at
                .map(|started| (session.config.agent.name(), started.elapsed()))
        });
        if let Some(form) = self.editing.as_ref().and_then(|id| self.forms.get(id)) {
            form.draw(frame, content);
            return SetCursorStyle::DefaultUserShape;
        }
        let pane_view = self.pane_view();
        // An agent that has not painted yet still shows its startup banner.
        let child = self
            .session()
            .and_then(|s| s.child(pane_view))
            .filter(|_| !(pane_view == View::Primary && starting.is_some()));
        let Some(child) = child else {
            let mut text = ui::details(display, "");
            if let Some(issue) = self.linear.issue() {
                let form = self.forms.get(&issue.id);
                let details = form.and_then(|form| form.details.as_ref());
                let comments: Vec<_> = details
                    .map(|details| {
                        details
                            .comments
                            .iter()
                            .map(|comment| ui::Comment {
                                author: comment
                                    .user
                                    .as_ref()
                                    .map_or("Unknown", |user| user.name.as_str()),
                                created_at: &comment.created_at,
                                body: &comment.body,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let status = if form.is_none_or(|form| form.busy || form.load_pending) {
                    Some("Loading comments…")
                } else if details.is_none() {
                    Some(form.map_or("Comments unavailable · r to retry", |form| {
                        form.message.as_str()
                    }))
                } else {
                    None
                };
                ui::append_comments(&mut text, &comments, status);
            }
            ui::draw_details(
                frame,
                content,
                text,
                self.view == View::Overview,
                starting,
                self.overview_scroll,
            );
            return SetCursorStyle::DefaultUserShape;
        };
        if let Some(selection) = &self.selection {
            frame.render_widget(selection, content);
            SetCursorStyle::DefaultUserShape
        } else {
            let parser = child.parser.lock().unwrap_or_else(|e| e.into_inner());
            let focused = self.view != View::Manager && !self.modal_open();
            ui::draw_terminal(frame, content, parser.screen(), focused);
            if focused {
                parser.callbacks().cursor_style
            } else {
                SetCursorStyle::DefaultUserShape
            }
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
        if self.activity_popup {
            ui::activity_popup(
                frame,
                self.linear
                    .issue()
                    .map_or("", |issue| issue.identifier.as_str()),
                self.activity_events(),
                self.activity_popup_scroll,
            );
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

    fn activity_events(&self) -> &[String] {
        self.editing
            .as_deref()
            .or_else(|| self.linear.issue().map(|issue| issue.id.as_str()))
            .and_then(|id| self.activity.get(id))
            .map_or(&[], Vec::as_slice)
    }

    fn open_activity(&mut self) {
        self.activity_popup = true;
        self.activity_popup_scroll = 0;
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
        } else if self.activity_popup {
            let limit =
                ui::activity_scroll_limit(Rect::new(0, 0, size.0, size.1), self.activity_events());
            self.activity_popup_scroll = self.activity_popup_scroll.min(limit);
            match key.code {
                KeyCode::Esc | KeyCode::Char('v' | 'q') => self.activity_popup = false,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.activity_popup_scroll =
                        self.activity_popup_scroll.saturating_add(1).min(limit);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.activity_popup_scroll = self.activity_popup_scroll.saturating_sub(1);
                }
                KeyCode::PageUp => {
                    self.activity_popup_scroll =
                        self.activity_popup_scroll.saturating_add(10).min(limit);
                }
                KeyCode::PageDown => {
                    self.activity_popup_scroll = self.activity_popup_scroll.saturating_sub(10);
                }
                KeyCode::Home => self.activity_popup_scroll = limit,
                KeyCode::End => self.activity_popup_scroll = 0,
                _ => {}
            }
        } else if self.status {
            self.status = !matches!(key.code, KeyCode::Esc | KeyCode::Char('s'));
        } else if self.help {
            self.help = !matches!(key.code, KeyCode::Esc | KeyCode::Char('?'));
        } else if let Some(id) = self.editing.clone() {
            if key.code == KeyCode::Esc || self.config.is_escape(key) {
                self.editing = None;
                self.view = View::Overview;
            } else if let Some(operation) = self.forms.get_mut(&id).and_then(|form| form.key(key)) {
                let started = self.linear.edit(id.clone(), operation);
                let form = self.forms.get_mut(&id).unwrap();
                form.busy = started;
                form.message = if started {
                    "Saving…"
                } else {
                    "Linear is busy or disconnected. Try again shortly."
                }
                .into();
            }
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
            KeyCode::Char('v') => self.open_activity(),
            KeyCode::Char('l') => self.linear.open_picker(),
            KeyCode::Char('r') => {
                self.linear.start(false);
                if let Some(id) = self.linear.issue().map(|i| i.id.clone()) {
                    self.forms.entry(id).or_default().load_pending = true;
                }
            }

            KeyCode::Char('o') => self.open_in_linear()?,
            KeyCode::Char('b') => self.create_bookmark()?,
            KeyCode::Char('p') => self.push_ticket()?,
            KeyCode::Char('j' | 'k') | KeyCode::Down | KeyCode::Up
                if self.view == View::Overview =>
            {
                self.overview_scroll = if matches!(key.code, KeyCode::Char('j') | KeyCode::Down) {
                    self.overview_scroll.saturating_add(1)
                } else {
                    self.overview_scroll.saturating_sub(1)
                };
            }
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
                self.activity_scroll = 0;
                self.overview_scroll = 0;
                if let Some(id) = self.linear.issue().map(|issue| issue.id.clone())
                    && let Some(form) = self.forms.get_mut(&id)
                    && !form.busy
                {
                    form.load_pending = self.config.linear.is_some();
                }
            }
            KeyCode::Char('i') => {
                if let Some(id) = self.linear.issue().map(|issue| issue.id.clone()) {
                    self.current = Some(id.clone());
                    self.view = View::Overview;
                    self.overview_scroll = 0;
                    let form = self.forms.entry(id).or_default();
                    if !form.busy {
                        form.load_pending = self.config.linear.is_some();
                    }
                }
            }
            KeyCode::Char('e') if self.view == View::Overview => {
                if let Some(id) = self.linear.issue().map(|i| i.id.clone()) {
                    let started = self.linear.edit(id.clone(), ::linear::EditOperation::Load);
                    let form = self.forms.entry(id.clone()).or_default();
                    form.load_pending = !started && !form.busy && self.config.linear.is_some();
                    form.busy |= started;
                    form.message = if form.busy || form.load_pending {
                        "Loading issue…"
                    } else {
                        "Connect Linear before editing this issue."
                    }
                    .into();
                    self.editing = Some(id);
                    self.view = View::Overview;
                }
            }
            KeyCode::Char('a') => self.open_ticket(View::Primary, false, size)?,
            KeyCode::Enter => self.open_ticket(View::Primary, true, size)?,
            KeyCode::Char('e') => self.open_ticket(View::Editor, false, size)?,
            KeyCode::Char('t') => self.open_ticket(View::Shell, false, size)?,
            _ => {}
        }
        Ok(false)
    }

    fn push_ticket(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.config.workspace == crate::settings::WorkspaceKind::Jj,
            "jj git push requires the jj workspace backend"
        );
        let issue = self
            .linear
            .issue()
            .context("Select a Linear ticket first")?
            .clone();
        anyhow::ensure!(
            !self.pushes.contains_key(&issue.id),
            "A push is already active for this ticket"
        );
        self.ensure_session(&issue)?;
        let directory = &self
            .sessions
            .get(&issue.id)
            .expect("ticket session exists")
            .config
            .cwd;
        let push = crate::workspace::Push::start(directory)?;
        self.pushes.insert(issue.id.clone(), push);
        self.notice = format!("Started jj git push for {}.", issue.identifier);
        self.record(issue.id, self.notice.clone());
        Ok(())
    }

    fn create_bookmark(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.config.workspace == crate::settings::WorkspaceKind::Jj,
            "Linear bookmarks require the jj workspace backend"
        );
        let issue = self
            .linear
            .issue()
            .context("Select a Linear ticket first")?
            .clone();
        let name = issue
            .branch_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .context("This issue has no Linear branch name; press r to refresh issues")?;
        self.ensure_session(&issue)?;
        let directory = &self
            .sessions
            .get(&issue.id)
            .expect("ticket session exists")
            .config
            .cwd;
        crate::workspace::create_bookmark(directory, name)?;
        self.notice = format!(
            "DONE\nSet bookmark {name}\nAt @ in {}; tracking origin.",
            issue.identifier
        );
        self.record(issue.id, self.notice.clone());
        Ok(())
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
        let id = issue.id.clone();
        let message = format!("Opening {} in Linear…", issue.identifier);
        self.link_openers.push((id.clone(), child));
        self.record(id, message);
        Ok(())
    }

    fn mouse(&mut self, mut mouse: MouseEvent, size: (u16, u16)) -> Result<MouseOutcome> {
        let activity = ui::areas(Rect::new(0, 0, size.0, size.1), self.sidebar_visible).activity;
        if activity.contains((mouse.column, mouse.row).into()) {
            let before = self.activity_scroll;
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.activity_scroll = self.activity_scroll.saturating_add(1).min(199)
                }
                MouseEventKind::ScrollDown => {
                    self.activity_scroll = self.activity_scroll.saturating_sub(1)
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    self.open_activity();
                    return Ok(MouseOutcome::redraw());
                }
                _ => {}
            }
            return Ok(MouseOutcome {
                copied: None,
                redraw: before != self.activity_scroll,
            });
        }
        if self.editing.is_some() {
            return Ok(MouseOutcome::default());
        }
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
            let before = parser.screen().scrollback();
            if input::scroll_history(parser.screen_mut(), mouse) {
                outcome.redraw |= parser.screen().scrollback() != before;
                return Ok(outcome);
            }
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
                            if let Some(id) = self
                                .linear
                                .issue()
                                .map(|issue| issue.id.clone())
                                .or_else(|| self.current.clone())
                            {
                                self.record(id, format!("FAILED\n{error:#}"));
                            } else {
                                self.notice = format!("{error:#}");
                            }
                            self.view = View::Manager;
                        }
                    }
                }
                Event::Mouse(mouse) if self.activity_popup => {
                    let key = match mouse.kind {
                        MouseEventKind::ScrollUp => Some(KeyCode::Up),
                        MouseEventKind::ScrollDown => Some(KeyCode::Down),
                        _ => None,
                    };
                    if let Some(key) = key {
                        let Some(size) = host_io(terminal::size())? else {
                            return Ok(());
                        };
                        self.key(KeyEvent::new(key, KeyModifiers::NONE), size)?;
                        redraw = true;
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
                    if let Some(form) = self.editing.as_ref().and_then(|id| self.forms.get_mut(id))
                    {
                        form.paste(&text);
                    } else if let Some(child) = self.session().and_then(|s| s.child(self.view)) {
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

fn record_event(activity: &mut HashMap<String, Vec<String>>, id: String, message: String) {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        % 86400;
    let events = activity.entry(id).or_default();
    if events.last().is_some_and(|event| event.ends_with(&message)) {
        return;
    }
    events.push(format!(
        "{:02}:{:02}:{:02} UTC  {}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        message
    ));
    if events.len() > 200 {
        events.remove(0);
    }
}
