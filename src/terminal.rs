use std::{
    io::{self, Stdout, stdout},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
#[cfg(unix)]
use signal_hook::{
    SigId,
    consts::{SIGHUP, SIGTERM},
};

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
    Signal(i32),
}

pub struct TerminalSession {
    active: bool,
}

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
            let _ = restore_terminal();
            return Err(error);
        }

        Ok(Self { active: true })
    }

    pub fn terminal(&self) -> io::Result<ZecTerminal> {
        Terminal::new(CrosstermBackend::new(stdout()))
    }
}

impl TerminalSession {
    pub fn restore(mut self) -> io::Result<()> {
        let result = restore_terminal();
        if result.is_ok() {
            self.active = false;
        }
        result
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.active {
            let _ = restore_terminal();
        }
    }
}

fn restore_terminal() -> io::Result<()> {
    let display_result = execute!(
        stdout(),
        Show,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let raw_result = disable_raw_mode();
    match (display_result, raw_result) {
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

struct ShutdownSignals {
    pending: Arc<AtomicUsize>,
    #[cfg(unix)]
    registrations: Vec<SigId>,
}

impl ShutdownSignals {
    fn register() -> io::Result<Self> {
        let pending = Arc::new(AtomicUsize::new(0));
        #[cfg(unix)]
        {
            let mut registrations = Vec::with_capacity(2);
            for signal in [SIGHUP, SIGTERM] {
                match signal_hook::flag::register_usize(signal, pending.clone(), signal as usize) {
                    Ok(registration) => registrations.push(registration),
                    Err(error) => {
                        for registration in registrations {
                            signal_hook::low_level::unregister(registration);
                        }
                        return Err(error);
                    }
                }
            }
            Ok(Self {
                pending,
                registrations,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self { pending })
        }
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        #[cfg(unix)]
        for registration in self.registrations.drain(..) {
            signal_hook::low_level::unregister(registration);
        }
    }
}

pub struct InputReader {
    stop: Arc<AtomicBool>,
    sender: Sender<TerminalEvent>,
    thread: Option<JoinHandle<()>>,
    _signals: ShutdownSignals,
}

impl InputReader {
    pub fn spawn(sender: Sender<TerminalEvent>) -> io::Result<Self> {
        let signals = ShutdownSignals::register()?;
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();
        let pending_signal = signals.pending.clone();
        let reader_sender = sender.clone();
        let thread = thread::Builder::new()
            .name("zec-terminal-input".to_owned())
            .spawn(move || read_events(reader_sender, reader_stop, pending_signal))?;

        Ok(Self {
            sender,
            stop,
            thread: Some(thread),
            _signals: signals,
        })
    }

    pub fn stop(&self) {
        self.sender.close();
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn stop_and_join(&mut self) {
        self.stop();
        self.join();
    }

    fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.stop();
        self.join();
    }
}

fn take_pending_signal(pending: &AtomicUsize) -> Option<i32> {
    let signal = pending.swap(0, Ordering::SeqCst);
    (signal != 0).then_some(signal as i32)
}

fn send_pending_signal(sender: &Sender<TerminalEvent>, pending: &AtomicUsize) -> bool {
    let Some(signal) = take_pending_signal(pending) else {
        return false;
    };
    let _ = sender.send_blocking(TerminalEvent::Signal(signal));
    true
}

fn read_events(
    sender: Sender<TerminalEvent>,
    stop: Arc<AtomicBool>,
    pending_signal: Arc<AtomicUsize>,
) {
    let mut known_size = crossterm_terminal::size().ok();
    let mut last_size_check = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        if send_pending_signal(&sender, &pending_signal) {
            break;
        }

        let polled = event::poll(Duration::from_millis(50));
        if send_pending_signal(&sender, &pending_signal) {
            break;
        }
        match polled {
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
                if !send_pending_signal(&sender, &pending_signal) {
                    let _ = sender.send_blocking(TerminalEvent::Error(error.to_string()));
                }
                break;
            }
        }

        let event = match event::read() {
            Ok(event) => event,
            Err(error) => {
                if !send_pending_signal(&sender, &pending_signal) {
                    let _ = sender.send_blocking(TerminalEvent::Error(error.to_string()));
                }
                break;
            }
        };
        if send_pending_signal(&sender, &pending_signal) {
            break;
        }

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
    fn pending_signal_is_forwarded_once() {
        let pending = AtomicUsize::new(15);
        let (sender, receiver) = async_channel::bounded(1);

        assert!(send_pending_signal(&sender, &pending));
        assert!(matches!(
            receiver.recv_blocking(),
            Ok(TerminalEvent::Signal(15))
        ));
        assert_eq!(pending.load(Ordering::SeqCst), 0);
        assert!(!send_pending_signal(&sender, &pending));
    }
    #[test]
    fn closing_a_full_channel_unblocks_a_blocking_sender() {
        let (sender, _receiver) = async_channel::bounded(1);
        sender
            .send_blocking(TerminalEvent::Redraw)
            .expect("fill event channel");
        let blocked_sender = sender.clone();
        let blocked = thread::spawn(move || blocked_sender.send_blocking(TerminalEvent::Resize));

        sender.close();
        assert!(blocked.join().expect("join blocked sender").is_err());
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
