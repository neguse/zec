use std::{
    io::{self, Stdout, stdout},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use async_channel::Sender;
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyEvent, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{
        self as crossterm_terminal, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub type ZecTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollDirection {
    Up,
    Down,
}

#[derive(Debug)]
pub enum TerminalEvent {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    MouseScroll(ScrollDirection),
    Resize,
    Redraw,
    ReloadFinished {
        buffer_id: u64,
        result: Result<(), String>,
    },
    Error(String),
}

pub struct TerminalSession;

impl TerminalSession {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;

        if let Err(error) = execute!(
            stdout(),
            EnterAlternateScreen,
            Clear(ClearType::All),
            EnableBracketedPaste,
            EnableMouseCapture,
            Hide
        ) {
            restore_terminal();
            return Err(error);
        }

        Ok(Self)
    }

    pub fn terminal(&self) -> io::Result<ZecTerminal> {
        Terminal::new(CrosstermBackend::new(stdout()))
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    let _ = execute!(
        stdout(),
        Show,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
}

pub struct InputReader {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl InputReader {
    pub fn spawn(sender: Sender<TerminalEvent>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();
        let thread = thread::spawn(move || read_events(sender, reader_stop));

        Self {
            stop,
            thread: Some(thread),
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_events(sender: Sender<TerminalEvent>, stop: Arc<AtomicBool>) {
    let mut known_size = crossterm_terminal::size().ok();
    let mut last_size_check = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        match event::poll(Duration::from_millis(50)) {
            Ok(false) => {
                if last_size_check.elapsed() >= Duration::from_millis(250) {
                    last_size_check = Instant::now();
                    if let Ok(size) = crossterm_terminal::size()
                        && known_size.replace(size) != Some(size)
                        && sender.send_blocking(TerminalEvent::Resize).is_err()
                    {
                        break;
                    }
                }
                continue;
            }
            Ok(true) => {}
            Err(error) => {
                let _ = sender.send_blocking(TerminalEvent::Error(error.to_string()));
                break;
            }
        }

        let event = match event::read() {
            Ok(event) => event,
            Err(error) => {
                let _ = sender.send_blocking(TerminalEvent::Error(error.to_string()));
                break;
            }
        };

        if let Event::Resize(columns, rows) = &event {
            known_size = Some((*columns, *rows));
        };
        let Some(event) = map_event(event) else {
            continue;
        };

        if sender.send_blocking(event).is_err() {
            break;
        }
    }
}

fn map_event(event: Event) -> Option<TerminalEvent> {
    match event {
        Event::Key(key) => Some(TerminalEvent::Key(key)),
        Event::Paste(text) => Some(TerminalEvent::Paste(text)),
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => Some(TerminalEvent::MouseScroll(ScrollDirection::Up)),
            MouseEventKind::ScrollDown => Some(TerminalEvent::MouseScroll(ScrollDirection::Down)),
            MouseEventKind::Down(_) | MouseEventKind::Drag(_) | MouseEventKind::Up(_) => {
                Some(TerminalEvent::Mouse(mouse))
            }
            _ => None,
        },
        Event::Resize(_, _) => Some(TerminalEvent::Resize),
        Event::FocusGained | Event::FocusLost => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton};

    use super::*;

    fn mouse_event(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 7,
            row: 11,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(mouse_event(kind))
    }

    #[test]
    fn maps_vertical_mouse_wheel_events() {
        assert!(matches!(
            map_event(mouse(MouseEventKind::ScrollUp)),
            Some(TerminalEvent::MouseScroll(ScrollDirection::Up))
        ));
        assert!(matches!(
            map_event(mouse(MouseEventKind::ScrollDown)),
            Some(TerminalEvent::MouseScroll(ScrollDirection::Down))
        ));
    }

    #[test]
    fn preserves_button_mouse_events() {
        let events = [
            mouse_event(MouseEventKind::Down(MouseButton::Left)),
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Right),
                column: 19,
                row: 23,
                modifiers: KeyModifiers::ALT | KeyModifiers::CONTROL,
            },
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Middle),
                column: 29,
                row: 31,
                modifiers: KeyModifiers::SHIFT,
            },
        ];

        for event in events {
            assert!(matches!(
                map_event(Event::Mouse(event)),
                Some(TerminalEvent::Mouse(mapped)) if mapped == event
            ));
        }
    }

    #[test]
    fn drops_unhandled_mouse_events() {
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            assert!(map_event(mouse(kind)).is_none(), "mapped {kind:?}");
        }
    }

    #[test]
    fn preserves_existing_non_mouse_event_mapping() {
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(matches!(
            map_event(Event::Key(key)),
            Some(TerminalEvent::Key(mapped)) if mapped == key
        ));
        assert!(matches!(
            map_event(Event::Paste("text".to_owned())),
            Some(TerminalEvent::Paste(text)) if text == "text"
        ));
        assert!(matches!(
            map_event(Event::Resize(80, 24)),
            Some(TerminalEvent::Resize)
        ));
        assert!(map_event(Event::FocusGained).is_none());
        assert!(map_event(Event::FocusLost).is_none());
    }
}
