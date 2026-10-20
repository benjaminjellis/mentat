use crate::{
    agent_session,
    config::Config,
    ticket::PendingPaste,
    workspace::{WorkspaceLock, Workspaces},
};
use ::terminal::pty::Session;
use anyhow::Result;
use crossterm::terminal;
use ratatui::layout::Rect;
use std::ffi::OsString;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq)]
pub enum View {
    Manager,
    Overview,
    Primary,
    Editor,
    Shell,
}

impl View {
    /// The child terminal a view shows, if it shows one at all.
    pub fn pane(self) -> Option<Pane> {
        match self {
            Self::Primary => Some(Pane::Primary),
            Self::Editor => Some(Pane::Editor),
            Self::Shell => Some(Pane::Shell),
            Self::Manager | Self::Overview => None,
        }
    }
}

/// The terminals a ticket can own. Indexing [`TicketSession::children`] by this
/// keeps every traversal in one place instead of naming the three slots by hand.
#[derive(Clone, Copy, PartialEq)]
pub enum Pane {
    Primary,
    Editor,
    Shell,
}

impl Pane {
    pub const ALL: [Self; 3] = [Self::Primary, Self::Editor, Self::Shell];

    pub fn view(self) -> View {
        match self {
            Self::Primary => View::Primary,
            Self::Editor => View::Editor,
            Self::Shell => View::Shell,
        }
    }
}

pub struct TicketSession {
    pub config: Config,
    pub children: [Option<Session>; 3],
    pub startup: Option<agent_session::Startup>,
    pub starting_at: Option<Instant>,
    pub last: View,
    pub paste: Option<PendingPaste>,
    pub _lock: WorkspaceLock,
}
impl TicketSession {
    pub fn pane(&self, pane: Pane) -> Option<&Session> {
        self.children[pane as usize].as_ref()
    }
    pub fn child(&self, view: View) -> Option<&Session> {
        self.pane(view.pane()?)
    }
    /// The view this session shows while the sidebar holds focus.
    pub fn pane_view(&self, view: View) -> View {
        if view == View::Manager {
            self.last
        } else {
            view
        }
    }
    /// Whether the content pane has anything to show, live or still starting.
    pub fn has_content(&self) -> bool {
        self.child(self.last).is_some() || self.startup.is_some()
    }
    pub fn has_children(&self) -> bool {
        self.startup.is_some() || self.children.iter().any(Option::is_some)
    }
    pub fn protected(&self) -> bool {
        self.pane(Pane::Editor).is_some()
            || self.pane(Pane::Shell).is_some()
            || (self.config.protect_child && self.pane(Pane::Primary).is_some())
    }
    pub fn shutdown(&mut self) -> Result<()> {
        self.startup = None;
        self.starting_at = None;
        let mut error = None;
        for child in &mut self.children {
            if let Some(mut child) = child.take()
                && let Err(e) = child.shutdown()
            {
                error.get_or_insert(e);
            }
        }
        error.map_or(Ok(()), Err)
    }

    /// Start a pane's terminal unless it is already running. Each pane knows
    /// which command it runs, so callers only name the pane.
    pub fn open_pane(&mut self, pane: Pane, area: Rect) -> Result<()> {
        if self.pane(pane).is_some() {
            return Ok(());
        }
        let command: Vec<OsString> = match pane {
            Pane::Primary => self.config.command.clone(),
            Pane::Editor => self.config.editor.command(),
            Pane::Shell => vec![std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into())],
        };
        self.children[pane as usize] = Some(Session::spawn(
            &command,
            &self.config.cwd,
            area.width,
            area.height,
        )?);
        Ok(())
    }

    /// Poll the agent's background preparation. Returns `true` once preparation
    /// finished and the agent's terminal has been spawned in its place.
    pub fn advance_startup(
        &mut self,
        id: &str,
        workspaces: &Workspaces,
        sidebar_visible: bool,
    ) -> Result<bool> {
        let Some(startup) = &self.startup else {
            return Ok(false);
        };
        let config = match startup.poll() {
            Ok(None) => return Ok(false),
            Ok(Some(config)) => config,
            Err(error) => {
                self.startup = None;
                self.starting_at = None;
                return Err(error);
            }
        };
        // Preparation is done. Adopt the command it chose, then let the startup
        // go: the pending ticket is read back before the slot is cleared.
        self.config = config;
        self.paste = self
            .startup
            .take()
            .and_then(|startup| startup.paste.as_ref().map(PendingPaste::new));
        match self.launch_prepared_agent(id, workspaces, sidebar_visible) {
            Ok(()) => Ok(true),
            Err(error) => {
                self.starting_at = None;
                Err(error)
            }
        }
    }

    fn launch_prepared_agent(
        &mut self,
        id: &str,
        workspaces: &Workspaces,
        sidebar_visible: bool,
    ) -> Result<()> {
        if let Some(conversation) = &self.config.resume {
            workspaces.save_conversation(id, self.config.agent.name(), conversation)?;
        }
        self.open_pane(
            Pane::Primary,
            content_area(terminal::size()?, sidebar_visible),
        )?;
        self.config.new_conversation = false;
        Ok(())
    }

    /// Drop the "starting" banner as soon as the agent paints something.
    pub fn note_first_output(&mut self, id: &str) -> bool {
        let Some(started) = self.starting_at else {
            return false;
        };
        let Some(primary) = self.children[Pane::Primary as usize].as_ref() else {
            return false;
        };
        let parser = primary.parser.lock().unwrap_or_else(|e| e.into_inner());
        if parser.screen().contents().trim().is_empty() {
            return false;
        }
        drop(parser);
        tracing::info!(
            ticket_id = %id,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Agent produced output"
        );
        self.starting_at = None;
        true
    }

    /// Hand the ticket to the agent once it signals that it accepts a paste.
    /// A paste only survives while its own pane is the one on screen.
    pub fn deliver_paste(&mut self, active: bool) -> Result<()> {
        let (Some(paste), Some(primary)) =
            (&self.paste, self.children[Pane::Primary as usize].as_ref())
        else {
            self.paste = None;
            return Ok(());
        };
        if !active {
            self.paste = None;
            return Ok(());
        }
        let result = paste.poll(primary);
        if !matches!(result, Ok(false)) {
            self.paste = None;
        }
        result.map(|_| ())
    }
}

/// The area a child terminal occupies at a given host size.
pub fn content_area(size: (u16, u16), sidebar_visible: bool) -> Rect {
    ui::areas(Rect::new(0, 0, size.0, size.1), sidebar_visible).content
}
