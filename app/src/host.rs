use anyhow::Result;
use crossterm::{
    cursor::{SetCursorStyle, Show},
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    terminal::{
        self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io::{self, Write};

/// The real terminal Mentat draws on, as opposed to the emulated child screens.
pub type Host = Terminal<CrosstermBackend<Box<dyn Write>>>;

/// Keep cell updates and the final cursor position in one visible frame.
pub fn draw_frame<W: Write>(
    host: &mut Terminal<CrosstermBackend<W>>,
    render: impl FnOnce(&mut ratatui::Frame) -> SetCursorStyle,
) -> io::Result<()> {
    queue!(host.backend_mut(), BeginSynchronizedUpdate)?;
    let mut style = SetCursorStyle::DefaultUserShape;
    let result = host.draw(|frame| style = render(frame)).map(|_| ());
    // End the update even when drawing fails, so the host does not stay frozen.
    let end = execute!(host.backend_mut(), style, EndSynchronizedUpdate);
    result.and(end)
}

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
            EndSynchronizedUpdate,
            PopKeyboardEnhancementFlags,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            SetCursorStyle::DefaultUserShape,
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{TerminalOptions, Viewport, layout::Rect, widgets::Paragraph};

    #[test]
    fn frame_groups_cells_and_cursor_in_one_update() {
        let mut output = Vec::new();
        let mut host = Terminal::with_options(
            CrosstermBackend::new(&mut output),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 20, 2)),
            },
        )
        .unwrap();
        draw_frame(&mut host, |frame| {
            frame.render_widget(Paragraph::new("scroll"), frame.area());
            frame.set_cursor_position((3, 0));
            SetCursorStyle::SteadyBar
        })
        .unwrap();
        drop(host);
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("\x1b[?2026h"));
        assert!(output.contains("scroll"));
        assert!(output.contains("\x1b[1;4H"));
        assert!(output.contains("\x1b[6 q"));
        assert!(output.ends_with("\x1b[?2026l"));
    }
}
