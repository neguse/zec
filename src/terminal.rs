use std::{
    env,
    ffi::OsString,
    io::{self, Stdout, stdout},
    path::PathBuf,
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
        self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, KeyEvent, KeyboardEnhancementFlags,
        MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        self as crossterm_terminal, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use dap::RunInTerminalRequestArguments;
use futures::channel::mpsc as futures_mpsc;
use lsp::{LanguageServerId, WorkspaceEdit};
use project::CodeAction;
use ratatui::{Terminal, backend::CrosstermBackend};
use settings::WorktreeId;
#[cfg(unix)]
use signal_hook::{
    SigId,
    consts::{SIGHUP, SIGINT, SIGQUIT, SIGSTOP, SIGTERM, SIGTSTP},
};

use crate::{ProjectSearchRequestKey, actions::TerminalAction, repository::ProjectSearchOutput};

pub type ZecTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardProtocol {
    Kitty,
    ModifyOtherKeys,
    Legacy,
}

impl KeyboardProtocol {
    fn label(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::ModifyOtherKeys => "modifyOtherKeys",
            Self::Legacy => "legacy+F1",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalColorCapability {
    TrueColor,
    Ansi256,
    Ansi16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalImageProtocol {
    Kitty,
    Iterm2,
    Sixel,
    None,
}

impl TerminalImageProtocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::Iterm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::None => "none",
        }
    }
}

impl TerminalColorCapability {
    fn label(self) -> &'static str {
        match self {
            Self::TrueColor => "truecolor",
            Self::Ansi256 => "256-color",
            Self::Ansi16 => "16-color",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalCapabilities {
    pub keyboard: KeyboardProtocol,
    pub color: TerminalColorCapability,
    pub utf8: bool,
    pub mouse_motion_requested: bool,
    pub focus_requested: bool,
    pub osc52_attempted: bool,
    pub osc8_available: bool,
    pub image_protocol: TerminalImageProtocol,
    pub external_media_available: bool,
    pub mouse_observed: bool,
    pub focus_observed: bool,
}

impl TerminalCapabilities {
    pub fn detect() -> Self {
        Self::detect_with(|name| env::var_os(name))
    }

    fn detect_with(mut value: impl FnMut(&str) -> Option<OsString>) -> Self {
        let text = |name: &str, value: &mut dyn FnMut(&str) -> Option<OsString>| {
            value(name).map(|value| value.to_string_lossy().to_ascii_lowercase())
        };
        let term = text("TERM", &mut value).unwrap_or_default();
        let term_program = text("TERM_PROGRAM", &mut value).unwrap_or_default();
        let override_protocol = text("ZEC_KEYBOARD_PROTOCOL", &mut value);
        let inside_tmux = value("TMUX").is_some();
        let kitty_environment = value("KITTY_WINDOW_ID").is_some()
            || value("WEZTERM_PANE").is_some()
            || term.contains("kitty")
            || term.starts_with("foot")
            || matches!(term_program.as_str(), "wezterm" | "ghostty");
        let modify_other_keys_environment = value("XTERM_VERSION").is_some()
            || term.starts_with("xterm")
            || matches!(term_program.as_str(), "iterm.app" | "apple_terminal");
        let keyboard = match override_protocol.as_deref() {
            Some("kitty") => KeyboardProtocol::Kitty,
            Some("modifyotherkeys" | "modify_other_keys" | "xterm") => {
                KeyboardProtocol::ModifyOtherKeys
            }
            Some("legacy") => KeyboardProtocol::Legacy,
            _ if kitty_environment && !inside_tmux => KeyboardProtocol::Kitty,
            _ if modify_other_keys_environment && !inside_tmux => KeyboardProtocol::ModifyOtherKeys,
            _ => KeyboardProtocol::Legacy,
        };
        let color_term = text("COLORTERM", &mut value).unwrap_or_default();
        let color = if matches!(color_term.as_str(), "truecolor" | "24bit") {
            TerminalColorCapability::TrueColor
        } else if term.contains("256color") {
            TerminalColorCapability::Ansi256
        } else {
            TerminalColorCapability::Ansi16
        };
        let locale = text("LC_ALL", &mut value)
            .filter(|locale| !locale.is_empty())
            .or_else(|| text("LC_CTYPE", &mut value).filter(|locale| !locale.is_empty()))
            .or_else(|| text("LANG", &mut value))
            .unwrap_or_default();
        let usable_terminal = term != "dumb";
        let image_override = text("ZEC_IMAGE_PROTOCOL", &mut value);
        let image_protocol = match image_override.as_deref() {
            Some("kitty") => TerminalImageProtocol::Kitty,
            Some("iterm" | "iterm2" | "osc1337") => TerminalImageProtocol::Iterm2,
            Some("sixel") => TerminalImageProtocol::Sixel,
            Some("none" | "off" | "text") => TerminalImageProtocol::None,
            Some(_) => TerminalImageProtocol::None,
            None if !usable_terminal || inside_tmux => TerminalImageProtocol::None,
            None if kitty_environment => TerminalImageProtocol::Kitty,
            None if term_program == "iterm.app" => TerminalImageProtocol::Iterm2,
            None if term.contains("sixel") || value("DEC_SIXEL").is_some() => {
                TerminalImageProtocol::Sixel
            }
            None => TerminalImageProtocol::None,
        };
        let external_media_available = value("ZEC_EXTERNAL_MEDIA")
            .is_some_and(|value| value != "0" && !value.is_empty())
            || value("DISPLAY").is_some()
            || value("WAYLAND_DISPLAY").is_some()
            || cfg!(target_os = "macos")
            || cfg!(windows);

        Self {
            keyboard,
            color,
            utf8: locale.contains("utf-8") || locale.contains("utf8"),
            mouse_motion_requested: usable_terminal,
            focus_requested: usable_terminal,
            osc52_attempted: usable_terminal,
            osc8_available: usable_terminal,
            image_protocol,
            external_media_available,
            mouse_observed: false,
            focus_observed: false,
        }
    }

    pub fn observe_mouse(&mut self) {
        self.mouse_observed = true;
    }

    pub fn observe_focus(&mut self) {
        self.focus_observed = true;
    }

    pub fn summary(&self) -> String {
        format!(
            "terminal keyboard={} mouse={}/{} focus={}/{} OSC52={} OSC8={} image={} external-media={} color={} UTF-8={}",
            self.keyboard.label(),
            if self.mouse_motion_requested {
                "on"
            } else {
                "off"
            },
            if self.mouse_observed {
                "seen"
            } else {
                "unverified"
            },
            if self.focus_requested { "on" } else { "off" },
            if self.focus_observed {
                "seen"
            } else {
                "unverified"
            },
            if self.osc52_attempted {
                "attempt"
            } else {
                "off"
            },
            if self.osc8_available {
                "available"
            } else {
                "off"
            },
            self.image_protocol.label(),
            if self.external_media_available {
                "available"
            } else {
                "off"
            },
            self.color.label(),
            if self.utf8 { "yes" } else { "no" },
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollDirection {
    Up,
    Down,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionPresentation {
    pub label: String,
    pub detail: Option<String>,
    pub kind: Option<String>,
    pub documentation: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HoverPresentation {
    pub kind: String,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticPresentation {
    pub path: PathBuf,
    pub label: String,
    pub row: u32,
    pub column: u32,
    pub severity: String,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocationRequestKind {
    Definition,
    TypeDefinition,
    References,
    ProjectSymbols,
}

impl LocationRequestKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Definition => "Definitions",
            Self::TypeDefinition => "Type Definitions",
            Self::References => "References",
            Self::ProjectSymbols => "Project Symbols",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocationPresentation {
    pub path: PathBuf,
    pub label: String,
    pub row: u32,
    pub column: u32,
    pub end_row: u32,
    pub end_column: u32,
    pub snippet: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenamePreparation {
    pub placeholder: String,
    pub start: usize,
    pub end: usize,
}

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
    fn event(&self) -> Option<TerminalEvent> {
        self.pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(TerminalEvent::Resize {
            _acknowledgement: ResizeAcknowledgement {
                pending: self.pending.clone(),
            },
        })
    }
}

#[derive(Debug)]
pub enum TerminalEvent {
    Action(TerminalAction),
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    MouseScroll(ScrollDirection),
    FocusChanged(bool),
    Resize {
        _acknowledgement: ResizeAcknowledgement,
    },
    Redraw,
    /// The remote transport status or terminal askpass prompt changed.
    RemoteChanged,
    ExtensionsFetched {
        generation: u64,
        result: Result<Vec<crate::extension_picker::ExtensionRecord>, String>,
    },
    ExtensionStoreChanged {
        message: String,
    },
    ZedTerminalClosed {
        entity_id: u64,
    },
    TaskFinished {
        entity_id: u64,
        success: bool,
        hide: task::HideStrategy,
    },
    AgentTerminalAuthenticationFinished {
        entity_id: u64,
        method_id: String,
        success: bool,
    },
    InlineAssistChunk {
        generation: u64,
        chunk: String,
    },
    InlineAssistFinished {
        generation: u64,
        result: Result<String, String>,
    },
    DapNotification(String),
    DapRunInTerminal {
        session: gpui::Entity<project::debugger::session::Session>,
        request: RunInTerminalRequestArguments,
        sender: futures_mpsc::Sender<anyhow::Result<u32>>,
    },
    OutlineChanged {
        buffer_id: u64,
    },
    ReloadFinished {
        buffer_id: u64,
        result: Result<(), String>,
    },
    ProjectSearchDebounceElapsed {
        request: ProjectSearchRequestKey,
    },
    ProjectSearchFinished {
        request: ProjectSearchRequestKey,
        result: Result<ProjectSearchOutput, String>,
    },
    CompletionFinished {
        buffer_id: u64,
        generation: u64,
        menu_wait_attempt: u16,
        result: Result<Vec<CompletionPresentation>, String>,
    },
    HoverFinished {
        buffer_id: u64,
        generation: u64,
        result: Result<Vec<HoverPresentation>, String>,
    },
    DiagnosticsFinished {
        generation: u64,
        result: Result<Vec<DiagnosticPresentation>, String>,
    },
    LocationsFinished {
        buffer_id: u64,
        generation: u64,
        kind: LocationRequestKind,
        result: Result<Vec<LocationPresentation>, String>,
    },
    RenamePrepared {
        buffer_id: u64,
        generation: u64,
        result: Result<RenamePreparation, String>,
    },
    RenamePreviewFinished {
        buffer_id: u64,
        generation: u64,
        new_name: String,
        result: Result<(LanguageServerId, WorkspaceEdit), String>,
    },
    CodeActionsFinished {
        buffer_id: u64,
        generation: u64,
        result: Result<Vec<CodeAction>, String>,
    },
    ConfigurationReloaded {
        kind: &'static str,
        result: Result<String, String>,
    },
    LanguageServiceNotice {
        level: &'static str,
        message: String,
    },
    WorktreeTrustRequired {
        worktree_id: WorktreeId,
        path: PathBuf,
    },
    Error(String),
    Signal(i32),
}

pub struct TerminalSession {
    active: bool,
    capabilities: TerminalCapabilities,
}

impl TerminalSession {
    pub fn enter() -> io::Result<Self> {
        let capabilities = TerminalCapabilities::detect();
        Self::activate(&capabilities)?;
        Ok(Self {
            active: true,
            capabilities,
        })
    }

    fn activate(capabilities: &TerminalCapabilities) -> io::Result<()> {
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
            let _ = restore_terminal(capabilities);
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
            let _ = restore_terminal(capabilities);
            return Err(error);
        }
        Ok(())
    }

    pub fn terminal(&self) -> io::Result<ZecTerminal> {
        Terminal::new(CrosstermBackend::new(stdout()))
    }

    pub fn capabilities(&self) -> &TerminalCapabilities {
        &self.capabilities
    }
}

impl TerminalSession {
    pub fn restore(mut self) -> io::Result<()> {
        let result = restore_terminal(&self.capabilities);
        if result.is_ok() {
            self.active = false;
        }
        result
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.active {
            let _ = restore_terminal(&self.capabilities);
        }
    }
}

fn restore_terminal(capabilities: &TerminalCapabilities) -> io::Result<()> {
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

pub fn suspend_and_resume(
    terminal: &mut ZecTerminal,
    capabilities: &TerminalCapabilities,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        restore_terminal(capabilities)?;
        signal_hook::low_level::raise(SIGSTOP)?;
        TerminalSession::activate(capabilities)?;
        // The physical screen is already clear. Reset Ratatui's previous
        // buffer without querying the cursor, so the next draw is complete.
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
    // Shutdown signals terminate the reader; SIGTSTP must keep it alive so
    // keyboard input resumes after SIGCONT.
    !is_suspend_signal(signal)
}

const RESIZE_BURST_QUIET_PERIOD: Duration = Duration::from_millis(4);
const RESIZE_BURST_LIMIT: Duration = Duration::from_millis(25);

fn coalesce_resize_runs(events: impl IntoIterator<Item = Event>) -> Vec<Event> {
    let mut compacted = Vec::new();
    let mut pending_resize = None;
    for event in events {
        match event {
            resize @ Event::Resize(_, _) => pending_resize = Some(resize),
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

fn read_resize_burst(first: Event) -> io::Result<Vec<Event>> {
    if !matches!(first, Event::Resize(_, _)) {
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
        let follows_resize_run = matches!(next, Event::Resize(_, _));
        events.push(next);
        if !follows_resize_run {
            break;
        }
    }
    Ok(coalesce_resize_runs(events))
}

fn read_events(
    sender: Sender<TerminalEvent>,
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
                if last_size_check.elapsed() >= Duration::from_millis(250) {
                    last_size_check = Instant::now();
                    if let Ok(size) = crossterm_terminal::size()
                        && known_size.replace(size) != Some(size)
                        && let Some(event) = resize_coalescer.event()
                        && sender.send_blocking(event).is_err()
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

        let events = match event::read().and_then(read_resize_burst) {
            Ok(events) => events,
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

        for event in events {
            if let Event::Resize(columns, rows) = &event {
                known_size = Some((*columns, *rows));
            };
            let Some(event) = map_event_with_resize(event, &resize_coalescer) else {
                continue;
            };

            if sender.send_blocking(event).is_err() {
                return;
            }
        }
    }
}

fn map_event_with_resize(
    event: Event,
    resize_coalescer: &ResizeCoalescer,
) -> Option<TerminalEvent> {
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
        Event::Resize(_, _) => resize_coalescer.event(),
        Event::FocusGained => Some(TerminalEvent::FocusChanged(true)),
        Event::FocusLost => Some(TerminalEvent::FocusChanged(false)),
    }
}

#[cfg(test)]
fn map_event(event: Event) -> Option<TerminalEvent> {
    map_event_with_resize(event, &ResizeCoalescer::default())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

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

    #[cfg(unix)]
    #[test]
    fn suspend_signal_is_forwarded_without_stopping_the_reader() {
        let pending = AtomicUsize::new(SIGTSTP as usize);
        let (sender, receiver) = async_channel::bounded(1);

        assert!(!send_pending_signal(&sender, &pending));
        assert!(matches!(
            receiver.recv_blocking(),
            Ok(TerminalEvent::Signal(SIGTSTP))
        ));
        assert_eq!(pending.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn coalesces_resizes_until_the_queued_event_is_consumed() {
        let resize_coalescer = ResizeCoalescer::default();
        let first = resize_coalescer
            .event()
            .expect("first resize must be queued");
        assert!(resize_coalescer.event().is_none());

        drop(first);
        assert!(matches!(
            resize_coalescer.event(),
            Some(TerminalEvent::Resize { .. })
        ));
    }

    #[test]
    fn resize_burst_keeps_the_last_size_before_following_input() {
        let key = KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE);
        let compacted = coalesce_resize_runs([
            Event::Resize(1, 1),
            Event::Resize(80, 24),
            Event::Resize(1, 1),
            Event::Resize(120, 40),
            Event::Key(key),
        ]);

        assert_eq!(compacted.len(), 2);
        assert!(matches!(compacted[0], Event::Resize(120, 40)));
        assert!(matches!(compacted[1], Event::Key(actual) if actual == key));
    }

    #[test]
    fn closing_a_full_channel_unblocks_a_blocking_sender() {
        let (sender, _receiver) = async_channel::bounded(1);
        sender
            .send_blocking(TerminalEvent::Redraw)
            .expect("fill event channel");
        let blocked_sender = sender.clone();
        let resize = ResizeCoalescer::default()
            .event()
            .expect("create the pending resize event");
        let blocked = thread::spawn(move || blocked_sender.send_blocking(resize));

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
    fn preserves_focus_events() {
        assert!(matches!(
            map_event(Event::FocusGained),
            Some(TerminalEvent::FocusChanged(true))
        ));
        assert!(matches!(
            map_event(Event::FocusLost),
            Some(TerminalEvent::FocusChanged(false))
        ));
    }

    #[test]
    fn capability_detection_is_conservative_and_overrideable() {
        let detect = |pairs: &[(&str, &str)]| {
            let values = pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
                .collect::<BTreeMap<_, _>>();
            TerminalCapabilities::detect_with(|name| values.get(name).cloned())
        };

        let kitty = detect(&[
            ("TERM", "xterm-kitty"),
            ("COLORTERM", "truecolor"),
            ("LANG", "C.UTF-8"),
            ("KITTY_WINDOW_ID", "1"),
        ]);
        assert_eq!(kitty.keyboard, KeyboardProtocol::Kitty);
        assert_eq!(kitty.color, TerminalColorCapability::TrueColor);
        assert_eq!(kitty.image_protocol, TerminalImageProtocol::Kitty);
        assert!(kitty.utf8 && kitty.mouse_motion_requested && kitty.focus_requested);

        let multiplexed = detect(&[
            ("TERM", "xterm-kitty"),
            ("KITTY_WINDOW_ID", "1"),
            ("TMUX", "/tmp/tmux"),
        ]);
        assert_eq!(multiplexed.keyboard, KeyboardProtocol::Legacy);
        assert_eq!(multiplexed.image_protocol, TerminalImageProtocol::None);

        let iterm = detect(&[("TERM", "xterm-256color"), ("TERM_PROGRAM", "iTerm.app")]);
        assert_eq!(iterm.image_protocol, TerminalImageProtocol::Iterm2);

        let sixel = detect(&[("TERM", "xterm-256color"), ("ZEC_IMAGE_PROTOCOL", "sixel")]);
        assert_eq!(sixel.image_protocol, TerminalImageProtocol::Sixel);

        let invalid_image = detect(&[("ZEC_IMAGE_PROTOCOL", "unknown")]);
        assert_eq!(invalid_image.image_protocol, TerminalImageProtocol::None);

        let overridden = detect(&[("TERM", "dumb"), ("ZEC_KEYBOARD_PROTOCOL", "kitty")]);
        assert_eq!(overridden.keyboard, KeyboardProtocol::Kitty);
        assert!(!overridden.mouse_motion_requested);
        assert!(!overridden.focus_requested);
        assert!(!overridden.osc52_attempted);
        assert!(!overridden.osc8_available);
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
            Some(TerminalEvent::Resize { .. })
        ));
        assert!(matches!(
            map_event(Event::FocusGained),
            Some(TerminalEvent::FocusChanged(true))
        ));
        assert!(matches!(
            map_event(Event::FocusLost),
            Some(TerminalEvent::FocusChanged(false))
        ));
    }
}
