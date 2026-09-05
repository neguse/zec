//! Raw-mode session lifecycle: enter, restore, suspend, and resume.

use std::io::{self, Stdout, stdout};

use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use ratatui::backend::CrosstermBackend;
#[cfg(unix)]
use signal_hook::consts::{SIGSTOP, SIGTSTP};

use super::capabilities::{Capabilities, KeyboardProtocol};

pub type Terminal = ratatui::Terminal<CrosstermBackend<Stdout>>;

/// Owns the terminal modes for the lifetime of the interactive session.
///
/// Dropping the session restores every mode; [`Session::restore`] does the
/// same but reports the first failure instead of swallowing it.
pub struct Session {
    active: bool,
    capabilities: Capabilities,
}

impl Session {
    pub fn enter() -> io::Result<Self> {
        let capabilities = Capabilities::detect();
        activate(&capabilities)?;
        Ok(Self {
            active: true,
            capabilities,
        })
    }

    pub fn terminal(&self) -> io::Result<Terminal> {
        ratatui::Terminal::new(CrosstermBackend::new(stdout()))
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    pub fn restore(mut self) -> io::Result<()> {
        let result = restore(&self.capabilities);
        if result.is_ok() {
            self.active = false;
        }
        result
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.active {
            let _ = restore(&self.capabilities);
        }
    }
}

fn activate(capabilities: &Capabilities) -> io::Result<()> {
    enable_raw_mode()?;
    if let Err(error) = execute!(
        stdout(),
        EnterAlternateScreen,
        Clear(ClearType::All),
        EnableBracketedPaste,
        EnableFocusChange,
        EnableMouseCapture,
        Hide
    ) {
        let _ = restore(capabilities);
        return Err(error);
    }
    let keyboard_result = match capabilities.keyboard {
        KeyboardProtocol::Kitty => execute!(
            stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        ),
        KeyboardProtocol::ModifyOtherKeys => {
            use std::io::Write as _;
            let mut output = stdout();
            output
                .write_all(b"\x1b[>4;2m")
                .and_then(|()| output.flush())
        }
        KeyboardProtocol::Legacy => Ok(()),
    };
    if let Err(error) = keyboard_result {
        let _ = restore(capabilities);
        return Err(error);
    }
    Ok(())
}

/// Attempts every restoration step even when an earlier one fails, and
/// reports the first failure.
fn restore(capabilities: &Capabilities) -> io::Result<()> {
    let keyboard_result = match capabilities.keyboard {
        KeyboardProtocol::Kitty => execute!(stdout(), PopKeyboardEnhancementFlags),
        KeyboardProtocol::ModifyOtherKeys => {
            use std::io::Write as _;
            let mut output = stdout();
            output.write_all(b"\x1b[>4m").and_then(|()| output.flush())
        }
        KeyboardProtocol::Legacy => Ok(()),
    };
    let display_result = execute!(
        stdout(),
        Show,
        DisableMouseCapture,
        DisableFocusChange,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let raw_result = disable_raw_mode();
    match (keyboard_result, display_result, raw_result) {
        (Err(error), _, _) | (Ok(()), Err(error), _) | (Ok(()), Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(()), Ok(())) => Ok(()),
    }
}

pub fn is_suspend_signal(signal: i32) -> bool {
    #[cfg(unix)]
    {
        signal == SIGTSTP
    }
    #[cfg(not(unix))]
    {
        let _ = signal;
        false
    }
}

/// Restores the terminal, stops the process, and re-enters raw mode after
/// `SIGCONT`. The physical screen is clear afterwards, so Ratatui's previous
/// buffer is reset without querying the cursor.
pub fn suspend_and_resume(terminal: &mut Terminal, capabilities: &Capabilities) -> io::Result<()> {
    #[cfg(unix)]
    {
        restore(capabilities)?;
        signal_hook::low_level::raise(SIGSTOP)?;
        activate(capabilities)?;
        terminal.swap_buffers();
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (terminal, capabilities);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "job control is unavailable on this platform",
        ))
    }
}

/// Forces the next frame to repaint every cell. Used after a resize, when the
/// terminal may have discarded or reflowed the previous contents.
pub fn invalidate(terminal: &mut Terminal) -> io::Result<()> {
    use ratatui::backend::Backend as _;
    terminal.backend_mut().clear()?;
    terminal.swap_buffers();
    Ok(())
}

#[cfg(test)]
mod tests {
    use ratatui::{
        Terminal,
        backend::{Backend as _, TestBackend},
        widgets::Paragraph,
    };

    #[test]
    fn invalidation_forces_a_full_redraw() {
        let mut terminal = Terminal::new(TestBackend::new(8, 1)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("before"), frame.area()))
            .unwrap();
        terminal.backend_mut().clear().unwrap();
        terminal.swap_buffers();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("after"), frame.area()))
            .unwrap();
        let row = terminal.backend().buffer().content()[..8]
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(row, "after   ");
    }
}
