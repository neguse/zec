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
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEvent},
    execute,
    terminal::{
        self as crossterm_terminal, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub type ZecTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug)]
pub enum TerminalEvent {
    Key(KeyEvent),
    Paste(String),
    Resize,
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
            Hide
        ) {
            let _ = disable_raw_mode();
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
        let _ = execute!(stdout(), Show, DisableBracketedPaste, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
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

        let event = match event {
            Event::Key(key) => TerminalEvent::Key(key),
            Event::Paste(text) => TerminalEvent::Paste(text),
            Event::Resize(columns, rows) => {
                known_size = Some((columns, rows));
                TerminalEvent::Resize
            }
            Event::FocusGained | Event::FocusLost | Event::Mouse(_) => continue,
        };

        if sender.send_blocking(event).is_err() {
            break;
        }
    }
}
