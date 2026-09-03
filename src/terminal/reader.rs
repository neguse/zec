//! The blocking input reader thread and the events it produces.
//!
//! Input events apply backpressure so their order is preserved; resize
//! events coalesce until the previous one has been consumed. Signal handlers
//! only set an atomic flag, which the reader turns into [`Event::Signal`].

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use async_channel::Sender;
use crossterm::{
    event::{self, Event as CrosstermEvent, KeyEvent, MouseEvent, MouseEventKind},
    terminal as crossterm_terminal,
};
#[cfg(unix)]
use signal_hook::{
    SigId,
    consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGTSTP},
};

use super::session::is_suspend_signal;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollDirection {
    Up,
    Down,
}

/// Held by a resize event until the receiver drops it, so a burst of
/// resizes produces at most one queued event.
#[derive(Debug)]
pub struct ResizeAcknowledgement {
    pending: Arc<AtomicBool>,
}

impl Drop for ResizeAcknowledgement {
    fn drop(&mut self) {
        self.pending.store(false, Ordering::Release);
    }
}

#[derive(Clone, Debug, Default)]
struct ResizeCoalescer {
    pending: Arc<AtomicBool>,
}

impl ResizeCoalescer {
    fn event(&self) -> Option<Event> {
        self.pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Event::Resize(ResizeAcknowledgement {
            pending: self.pending.clone(),
        }))
    }
}

/// An event observed on the terminal.
#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Scroll(ScrollDirection),
    FocusChanged(bool),
    Resize(ResizeAcknowledgement),
    Signal(i32),
    /// The reader failed and has stopped.
    Error(String),
}

struct ShutdownSignals {
    pending: Arc<AtomicUsize>,
    #[cfg(unix)]
    registrations: Vec<SigId>,
}

impl ShutdownSignals {
    fn register() -> std::io::Result<Self> {
        let pending = Arc::new(AtomicUsize::new(0));
        #[cfg(unix)]
        {
            let mut registrations = Vec::with_capacity(5);
            for signal in [SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGTSTP] {
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

/// Reads Crossterm events on a dedicated thread and forwards them to the
/// application channel. Start it only after raw mode is active; starting it
/// earlier races a fast startup and can swallow the first keystroke.
pub struct InputReader {
    stop: Arc<AtomicBool>,
    close: Box<dyn Fn() + Send>,
    thread: Option<JoinHandle<()>>,
    _signals: ShutdownSignals,
}

impl InputReader {
    pub fn spawn<T>(sender: Sender<T>) -> std::io::Result<Self>
    where
        T: From<Event> + Send + 'static,
    {
        let signals = ShutdownSignals::register()?;
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();
        let pending_signal = signals.pending.clone();
        let reader_sender = sender.clone();
        let thread = thread::Builder::new()
            .name("zec-terminal-input".to_owned())
            .spawn(move || read_events(reader_sender, reader_stop, pending_signal))?;

        Ok(Self {
            stop,
            close: Box::new(move || {
                sender.close();
            }),
            thread: Some(thread),
            _signals: signals,
        })
    }

    pub fn stop_and_join(&mut self) {
        (self.close)();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn take_pending_signal(pending: &AtomicUsize) -> Option<i32> {
    let signal = pending.swap(0, Ordering::SeqCst);
    (signal != 0).then_some(signal as i32)
}

/// Forwards a pending signal. Returns true when the reader must stop.
fn send_pending_signal<T: From<Event>>(sender: &Sender<T>, pending: &AtomicUsize) -> bool {
    let Some(signal) = take_pending_signal(pending) else {
        return false;
    };
    let _ = sender.send_blocking(Event::Signal(signal).into());
    // Shutdown signals terminate the reader; SIGTSTP must keep it alive so
    // keyboard input resumes after SIGCONT.
    !is_suspend_signal(signal)
}

const RESIZE_BURST_QUIET_PERIOD: Duration = Duration::from_millis(4);
const RESIZE_BURST_LIMIT: Duration = Duration::from_millis(25);

fn coalesce_resize_runs(events: impl IntoIterator<Item = CrosstermEvent>) -> Vec<CrosstermEvent> {
    let mut compacted = Vec::new();
    let mut pending_resize = None;
    for event in events {
        match event {
            resize @ CrosstermEvent::Resize(_, _) => pending_resize = Some(resize),
            event => {
                if let Some(resize) = pending_resize.take() {
                    compacted.push(resize);
                }
                compacted.push(event);
            }
        }
    }
    if let Some(resize) = pending_resize {
        compacted.push(resize);
    }
    compacted
}

fn read_resize_burst(first: CrosstermEvent) -> std::io::Result<Vec<CrosstermEvent>> {
    if !matches!(first, CrosstermEvent::Resize(_, _)) {
        return Ok(vec![first]);
    }

    let started = Instant::now();
    let mut events = vec![first];
    loop {
        let remaining = RESIZE_BURST_LIMIT.saturating_sub(started.elapsed());
        if remaining.is_zero() || !event::poll(RESIZE_BURST_QUIET_PERIOD.min(remaining))? {
            break;
        }
        let next = event::read()?;
        let follows_resize_run = matches!(next, CrosstermEvent::Resize(_, _));
        events.push(next);
        if !follows_resize_run {
            break;
        }
    }
    Ok(coalesce_resize_runs(events))
}

fn read_events<T: From<Event>>(
    sender: Sender<T>,
    stop: Arc<AtomicBool>,
    pending_signal: Arc<AtomicUsize>,
) {
    let resize_coalescer = ResizeCoalescer::default();
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
                // Some PTYs never deliver SIGWINCH; poll the size instead.
                if last_size_check.elapsed() >= Duration::from_millis(250) {
                    last_size_check = Instant::now();
                    if let Ok(size) = crossterm_terminal::size()
                        && known_size.replace(size) != Some(size)
                        && let Some(event) = resize_coalescer.event()
                        && sender.send_blocking(event.into()).is_err()
                    {
                        break;
                    }
                }
                continue;
            }
            Ok(true) => {}
            Err(error) => {
                if !send_pending_signal(&sender, &pending_signal) {
                    let _ = sender.send_blocking(Event::Error(error.to_string()).into());
                }
                break;
            }
        }

        let events = match event::read().and_then(read_resize_burst) {
            Ok(events) => events,
            Err(error) => {
                if !send_pending_signal(&sender, &pending_signal) {
                    let _ = sender.send_blocking(Event::Error(error.to_string()).into());
                }
                break;
            }
        };
        if send_pending_signal(&sender, &pending_signal) {
            break;
        }

        for event in events {
            if let CrosstermEvent::Resize(columns, rows) = &event {
                known_size = Some((*columns, *rows));
            }
            let Some(event) = map_event(event, &resize_coalescer) else {
                continue;
            };
            if sender.send_blocking(event.into()).is_err() {
                return;
            }
        }
    }
}

fn map_event(event: CrosstermEvent, resize_coalescer: &ResizeCoalescer) -> Option<Event> {
    match event {
        CrosstermEvent::Key(key) => Some(Event::Key(key)),
        CrosstermEvent::Paste(text) => Some(Event::Paste(text)),
        CrosstermEvent::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => Some(Event::Scroll(ScrollDirection::Up)),
            MouseEventKind::ScrollDown => Some(Event::Scroll(ScrollDirection::Down)),
            MouseEventKind::Down(_) | MouseEventKind::Drag(_) | MouseEventKind::Up(_) => {
                Some(Event::Mouse(mouse))
            }
            _ => None,
        },
        CrosstermEvent::Resize(_, _) => resize_coalescer.event(),
        CrosstermEvent::FocusGained => Some(Event::FocusChanged(true)),
        CrosstermEvent::FocusLost => Some(Event::FocusChanged(false)),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton};

    use super::*;

    fn mouse(kind: MouseEventKind) -> CrosstermEvent {
        CrosstermEvent::Mouse(MouseEvent {
            kind,
            column: 7,
            row: 11,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn map(event: CrosstermEvent) -> Option<Event> {
        map_event(event, &ResizeCoalescer::default())
    }

    #[test]
    fn pending_signal_is_forwarded_once() {
        let pending = AtomicUsize::new(15);
        let (sender, receiver) = async_channel::bounded::<Event>(1);

        assert!(send_pending_signal(&sender, &pending));
        assert!(matches!(receiver.recv_blocking(), Ok(Event::Signal(15))));
        assert_eq!(pending.load(Ordering::SeqCst), 0);
        assert!(!send_pending_signal(&sender, &pending));
    }

    #[cfg(unix)]
    #[test]
    fn suspend_signal_is_forwarded_without_stopping_the_reader() {
        let pending = AtomicUsize::new(SIGTSTP as usize);
        let (sender, receiver) = async_channel::bounded::<Event>(1);

        assert!(!send_pending_signal(&sender, &pending));
        assert!(matches!(
            receiver.recv_blocking(),
            Ok(Event::Signal(SIGTSTP))
        ));
    }

    #[test]
    fn coalesces_resizes_until_the_queued_event_is_consumed() {
        let coalescer = ResizeCoalescer::default();
        let first = coalescer.event().expect("first resize must be queued");
        assert!(coalescer.event().is_none());
        drop(first);
        assert!(matches!(coalescer.event(), Some(Event::Resize(_))));
    }

    #[test]
    fn resize_burst_keeps_the_last_size_before_following_input() {
        let key = KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE);
        let compacted = coalesce_resize_runs([
            CrosstermEvent::Resize(1, 1),
            CrosstermEvent::Resize(80, 24),
            CrosstermEvent::Resize(1, 1),
            CrosstermEvent::Resize(120, 40),
            CrosstermEvent::Key(key),
        ]);

        assert_eq!(compacted.len(), 2);
        assert!(matches!(compacted[0], CrosstermEvent::Resize(120, 40)));
        assert!(matches!(compacted[1], CrosstermEvent::Key(actual) if actual == key));
    }

    #[test]
    fn closing_a_full_channel_unblocks_a_blocking_sender() {
        let (sender, _receiver) = async_channel::bounded::<Event>(1);
        sender
            .send_blocking(Event::Signal(0))
            .expect("fill event channel");
        let blocked_sender = sender.clone();
        let blocked = thread::spawn(move || blocked_sender.send_blocking(Event::Signal(1)));

        sender.close();
        assert!(blocked.join().expect("join blocked sender").is_err());
    }

    #[test]
    fn maps_mouse_wheel_buttons_focus_and_drops_motion() {
        assert!(matches!(
            map(mouse(MouseEventKind::ScrollUp)),
            Some(Event::Scroll(ScrollDirection::Up))
        ));
        assert!(matches!(
            map(mouse(MouseEventKind::ScrollDown)),
            Some(Event::Scroll(ScrollDirection::Down))
        ));
        assert!(matches!(
            map(mouse(MouseEventKind::Down(MouseButton::Left))),
            Some(Event::Mouse(_))
        ));
        assert!(map(mouse(MouseEventKind::Moved)).is_none());
        assert!(matches!(
            map(CrosstermEvent::FocusGained),
            Some(Event::FocusChanged(true))
        ));
        assert!(matches!(
            map(CrosstermEvent::Resize(80, 24)),
            Some(Event::Resize(_))
        ));
    }
}
