use anyhow::Result;
use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io::{self, Write};

/// The real terminal Mentat draws on, as opposed to the emulated child screens.
pub type Host = Terminal<CrosstermBackend<Box<dyn Write>>>;

/// Owns the host terminal's raw mode and alternate screen for as long as it
/// lives, so every exit path — including a panic — puts the terminal back.
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
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

pub fn host_io<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        // The host can close after the event loop's initial poll. Reads, writes,
        // and size queries may then fail before the next disconnection check.
        Err(_) if host_disconnected().unwrap_or(false) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn host_disconnected() -> io::Result<bool> {
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
